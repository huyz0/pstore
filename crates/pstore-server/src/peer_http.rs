//! The same bounded MessagePack peer operation over mTLS HTTP/2 and HTTP/3.

mod h3_guard;
mod pool;
pub(crate) use pool::Pool;

use crate::{
    PeerTls,
    peer_wire::{self, Accumulator, Reply, Request, WireError},
};
use axum::http::{self, StatusCode};
use bytes::{Buf, Bytes};
use futures_util::future::BoxFuture;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::{Serialize, de::DeserializeOwned};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

const PATH: &str = "/peer/v1/part";
const CONTENT: &str = "application/msgpack";
const HEADERS: u32 = 8192;
const STREAM_WINDOW: u32 = 64 * 1024;
const CONNECTION_WINDOW: u32 = 256 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const RPC_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub(crate) struct Limits {
    capacity: usize,
    connections: Arc<Semaphore>,
    handlers: Arc<Semaphore>,
    budget: Arc<Semaphore>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            capacity: 64,
            connections: Arc::new(Semaphore::new(64)),
            handlers: Arc::new(Semaphore::new(256)),
            budget: Arc::new(Semaphore::new(peer_wire::WIRE_BUDGET)),
        }
    }
}

impl Limits {
    pub(crate) fn connections(&self) -> usize {
        self.capacity - self.connections.available_permits()
    }
    #[cfg(test)]
    fn new(capacity: usize) -> Result<Arc<Self>, Error> {
        if capacity == 0 || capacity > 64 {
            return Err(Error::Protocol);
        }
        Ok(Arc::new(Self {
            capacity,
            connections: Arc::new(Semaphore::new(capacity)),
            ..Self::default()
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Protocol {
    H2,
    H3,
}

/// Which encrypted HTTP transport to use for newly established peer connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PeerTransport {
    /// Prefer HTTP/3, with availability-only fallback to HTTP/2.
    #[default]
    Auto,
    /// Require HTTP/2 over TLS/TCP.
    H2,
    /// Require HTTP/3 over QUIC/UDP.
    H3,
}

pub(crate) type Handler<Q = serde_json::Value, R = serde_json::Value> =
    Arc<dyn Fn(Protocol, Request<Q>) -> BoxFuture<'static, Reply<R>> + Send + Sync>;

/// Certificate failures remain distinct from transport availability.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("peer RPC deadline expired")]
    Deadline,
    #[error("peer is unavailable")]
    Unavailable,
    #[error("peer authentication failed")]
    Authentication,
    #[error("invalid peer protocol response")]
    Protocol,
    #[error(transparent)]
    Wire(#[from] WireError),
}

struct Driver(tokio::task::JoinHandle<()>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum Sender {
    H2(hyper::client::conn::http2::SendRequest<Full<Bytes>>),
    H3(h3::client::SendRequest<h3_guard::OpenStreams, Bytes>),
}

/// Cloneable stream senders never hold a lock across an RPC.
pub(crate) struct Client {
    sender: Sender,
    authority: String,
    _driver: Driver,
    _quic: Option<quinn::Endpoint>,
    budget: Arc<Semaphore>,
    slots: Semaphore,
}

impl Client {
    #[cfg(test)]
    pub(crate) async fn connect(
        address: SocketAddr,
        name: &str,
        tls: &PeerTls,
        protocol: Protocol,
        budget: Arc<Semaphore>,
    ) -> Result<Self, Error> {
        Self::connect_admitted(address, name, tls, protocol, budget, None).await
    }

    async fn connect_admitted(
        address: SocketAddr,
        name: &str,
        tls: &PeerTls,
        protocol: Protocol,
        budget: Arc<Semaphore>,
        admission: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<Self, Error> {
        let authority = if name.contains(':') {
            format!("[{name}]:{}", address.port())
        } else {
            format!("{name}:{}", address.port())
        };
        let (sender, driver, quic) = match protocol {
            Protocol::H2 => {
                let (_, config) = tls.configs_for(b"h2").map_err(|_| Error::Authentication)?;
                let socket = tokio::net::TcpStream::connect(address)
                    .await
                    .map_err(|_| Error::Unavailable)?;
                socket.set_nodelay(true).map_err(|_| Error::Unavailable)?;
                let name = rustls::pki_types::ServerName::try_from(name.to_owned())
                    .map_err(|_| Error::Authentication)?;
                let stream = tokio_rustls::TlsConnector::from(config)
                    .connect(name, socket)
                    .await
                    .map_err(|_| Error::Authentication)?;
                if stream.get_ref().1.alpn_protocol() != Some(b"h2") {
                    return Err(Error::Protocol);
                }
                let (sender, connection) =
                    hyper::client::conn::http2::Builder::new(TokioExecutor::new())
                        .max_header_list_size(HEADERS)
                        .initial_stream_window_size(Some(STREAM_WINDOW))
                        .initial_connection_window_size(Some(CONNECTION_WINDOW))
                        .adaptive_window(false)
                        .handshake(TokioIo::new(stream))
                        .await
                        .map_err(|_| Error::Authentication)?;
                let driver = tokio::spawn(async move {
                    let _admission = admission;
                    let _ = connection.await;
                });
                (Sender::H2(sender), driver, None)
            }
            Protocol::H3 => {
                let (_, config) = tls.configs_for(b"h3").map_err(|_| Error::Authentication)?;
                let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(config)
                    .map_err(|_| Error::Authentication)?;
                let bind = if address.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                };
                let mut endpoint =
                    quinn::Endpoint::client(bind.parse().map_err(|_| Error::Protocol)?)
                        .map_err(|_| Error::Unavailable)?;
                let mut config = quinn::ClientConfig::new(Arc::new(crypto));
                config.transport_config(quic_limits());
                endpoint.set_default_client_config(config);
                let connection = endpoint
                    .connect(address, name)
                    .map_err(|_| Error::Protocol)?
                    .await
                    .map_err(quic_error)?;
                let handshake = connection
                    .handshake_data()
                    .and_then(|h| h.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
                    .ok_or(Error::Authentication)?;
                if handshake.protocol.as_deref() != Some(b"h3") {
                    return Err(Error::Protocol);
                }
                let (mut driver, sender) = h3::client::builder()
                    .max_field_section_size(u64::from(HEADERS))
                    .build(h3_guard::Connection::new(connection))
                    .await
                    .map_err(|_| Error::Protocol)?;
                let driver = tokio::spawn(async move {
                    let _admission = admission;
                    let _ = futures_util::future::poll_fn(|cx| driver.poll_close(cx)).await;
                });
                (Sender::H3(sender), driver, Some(endpoint))
            }
        };
        Ok(Self {
            sender,
            authority,
            _driver: Driver(driver),
            _quic: quic,
            budget,
            slots: Semaphore::new(64),
        })
    }

    #[cfg(test)]
    pub(crate) async fn call(&self, request: Request) -> Result<Reply, Error> {
        self.call_typed(request).await
    }

    pub(crate) async fn call_typed<Q: Serialize + Send, R: DeserializeOwned + Send>(
        &self,
        request: Request<Q>,
    ) -> Result<Reply<R>, Error> {
        let _slot = self.slots.acquire().await.map_err(|_| Error::Unavailable)?;
        let id = request.id;
        let body = Bytes::from(peer_wire::encode(&request)?);
        drop(request);
        let uri = format!("https://{}{PATH}", self.authority);
        let build = || {
            http::Request::builder()
                .method("POST")
                .uri(uri.as_str())
                .header("content-type", CONTENT)
        };
        let (status, frame) = match &self.sender {
            Sender::H2(sender) => {
                let mut sender = sender.clone();
                sender.ready().await.map_err(|_| Error::Unavailable)?;
                let response = sender
                    .send_request(build().body(Full::new(body)).map_err(|_| Error::Protocol)?)
                    .await
                    .map_err(|_| Error::Unavailable)?;
                if response
                    .headers()
                    .get("content-type")
                    .and_then(|h| h.to_str().ok())
                    != Some(CONTENT)
                {
                    return Err(Error::Protocol);
                }
                let status = response.status().as_u16();
                (
                    status,
                    receive_h2(response.into_body(), self.budget.clone()).await?,
                )
            }
            Sender::H3(sender) => {
                let mut sender = sender.clone();
                let mut stream = sender
                    .send_request(build().body(()).map_err(|_| Error::Protocol)?)
                    .await
                    .map_err(|_| Error::Unavailable)?;
                stream
                    .send_data(body)
                    .await
                    .map_err(|_| Error::Unavailable)?;
                stream.finish().await.map_err(|_| Error::Unavailable)?;
                let response = stream
                    .recv_response()
                    .await
                    .map_err(|_| Error::Unavailable)?;
                if response
                    .headers()
                    .get("content-type")
                    .and_then(|h| h.to_str().ok())
                    != Some(CONTENT)
                {
                    return Err(Error::Protocol);
                }
                let status = response.status().as_u16();
                let mut body = Accumulator::default();
                while let Some(mut data) = stream.recv_data().await.map_err(|_| Error::Protocol)? {
                    let bytes = data.copy_to_bytes(data.remaining());
                    body.push(&bytes, self.budget.clone()).await?;
                }
                (status, body.finish()?)
            }
        };
        let reply: Reply<R> = frame.decode()?;
        if reply.id != id || reply.version != peer_wire::VERSION || reply.status != status {
            return Err(Error::Protocol);
        }
        Ok(reply)
    }
}

fn quic_error(error: quinn::ConnectionError) -> Error {
    match error {
        quinn::ConnectionError::TimedOut
        | quinn::ConnectionError::LocallyClosed
        | quinn::ConnectionError::Reset => Error::Unavailable,
        quinn::ConnectionError::ConnectionClosed(close)
            if matches!(
                close.error_code,
                quinn::TransportErrorCode::NO_ERROR
                    | quinn::TransportErrorCode::INTERNAL_ERROR
                    | quinn::TransportErrorCode::CONNECTION_REFUSED
            ) =>
        {
            Error::Unavailable
        }
        _ => Error::Authentication,
    }
}

fn quic_limits() -> Arc<quinn::TransportConfig> {
    let mut config = quinn::TransportConfig::default();
    config.max_concurrent_bidi_streams(64_u32.into());
    config.max_concurrent_uni_streams(3_u32.into());
    config.stream_receive_window(STREAM_WINDOW.into());
    config.receive_window(CONNECTION_WINDOW.into());
    config.max_idle_timeout(Some(quinn::IdleTimeout::from(quinn::VarInt::from_u32(
        30_000,
    ))));
    Arc::new(config)
}

async fn receive_h2(
    mut incoming: hyper::body::Incoming,
    budget: Arc<Semaphore>,
) -> Result<peer_wire::Frame, Error> {
    let mut body = Accumulator::default();
    while let Some(frame) = incoming.frame().await {
        let frame = frame.map_err(|_| Error::Protocol)?;
        if let Some(bytes) = frame.data_ref() {
            body.push(bytes, budget.clone()).await?;
        }
        if frame.trailers_ref().is_some() {
            return Err(Error::Protocol);
        }
    }
    Ok(body.finish()?)
}

fn valid_request<T>(request: &http::Request<T>) -> bool {
    request.method() == http::Method::POST
        && request.uri().path() == PATH
        && request.uri().query().is_none()
        && request
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            == Some(CONTENT)
        && !request.headers().contains_key("authorization")
}

async fn dispatch<Q: DeserializeOwned + Send + 'static, R: Serialize + Send + 'static>(
    protocol: Protocol,
    frame: peer_wire::Frame,
    handler: Handler<Q, R>,
) -> Result<Reply<R>, Error> {
    let request: Request<Q> = frame.decode()?;
    if request.version != peer_wire::VERSION
        || request.id == 0
        || request.tenant.is_empty()
        || !request.tenant.bytes().all(|b| b.is_ascii_digit())
        || request.tenant.parse::<u128>().is_err()
    {
        return Err(Error::Protocol);
    }
    Ok(handler(protocol, request).await)
}

fn response<R: Serialize>(reply: &Reply<R>) -> Result<http::Response<Full<Bytes>>, Error> {
    let body = peer_wire::encode(reply)?;
    http::Response::builder()
        .status(reply.status)
        .header("content-type", CONTENT)
        .body(Full::new(Bytes::from(body)))
        .map_err(|_| Error::Protocol)
}

async fn accept_tcp(listener: &tokio::net::TcpListener) -> Result<tokio::net::TcpStream, Error> {
    let (socket, _) = listener.accept().await.map_err(|_| Error::Unavailable)?;
    socket.set_nodelay(true).map_err(|_| Error::Unavailable)?;
    Ok(socket)
}

/// Dropping this future aborts all connection drivers. Neither listener exposes
/// a public API route, and both require certificates before accepting streams.
#[cfg(test)]
pub(crate) async fn serve(
    tcp: tokio::net::TcpListener,
    udp: std::net::UdpSocket,
    tls: PeerTls,
    handler: Handler,
) -> Result<(), Error> {
    serve_typed(tcp, udp, tls, handler).await
}

#[cfg(test)]
pub(crate) async fn serve_typed<
    Q: DeserializeOwned + Send + 'static,
    R: Serialize + Send + 'static,
>(
    tcp: tokio::net::TcpListener,
    udp: std::net::UdpSocket,
    tls: PeerTls,
    handler: Handler<Q, R>,
) -> Result<(), Error> {
    serve_with_limits(tcp, udp, tls, handler, Arc::new(Limits::default())).await
}

pub(crate) async fn serve_with_limits<
    Q: DeserializeOwned + Send + 'static,
    R: Serialize + Send + 'static,
>(
    tcp: tokio::net::TcpListener,
    udp: std::net::UdpSocket,
    tls: PeerTls,
    handler: Handler<Q, R>,
    limits: Arc<Limits>,
) -> Result<(), Error> {
    let (h2, _) = tls.configs_for(b"h2").map_err(|_| Error::Authentication)?;
    let (h3, _) = tls.configs_for(b"h3").map_err(|_| Error::Authentication)?;
    let crypto =
        quinn::crypto::rustls::QuicServerConfig::try_from(h3).map_err(|_| Error::Authentication)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(quic_limits());
    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(config),
        udp,
        Arc::new(quinn::TokioRuntime),
    )
    .map_err(|_| Error::Unavailable)?;
    let connections = limits.connections.clone();
    let handlers = limits.handlers.clone();
    let budget = limits.budget.clone();
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            socket = accept_tcp(&tcp) => {
                let socket = socket?;
                let Ok(permit) = connections.clone().try_acquire_owned() else {continue;};
                let (config,handler,handlers,budget) = (h2.clone(),handler.clone(),handlers.clone(),budget.clone());
                tasks.spawn(async move {
                    let _permit = permit;
                    let Ok(Ok(stream)) = tokio::time::timeout(HANDSHAKE_TIMEOUT,tokio_rustls::TlsAcceptor::from(config).accept(socket)).await else {return;};
                    if stream.get_ref().1.alpn_protocol() != Some(b"h2") {return;}
                    let service = hyper::service::service_fn(move |req: http::Request<hyper::body::Incoming>| {
                        let (handler,handlers,budget) = (handler.clone(),handlers.clone(),budget.clone());
                        async move {
                            tokio::time::timeout(RPC_TIMEOUT, async move {
                            if !valid_request(&req) {
                                return http::Response::builder().status(StatusCode::BAD_REQUEST).body(Full::new(Bytes::new())).map_err(|_| Error::Protocol);
                            }
                            let _permit = handlers.acquire().await.map_err(|_| Error::Unavailable)?;
                            let frame = receive_h2(req.into_body(),budget).await?;
                            response(&dispatch(Protocol::H2,frame,handler).await?)
                            }).await.map_err(|_| Error::Unavailable)?
                        }
                    });
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .max_concurrent_streams(64).max_header_list_size(HEADERS)
                        .initial_stream_window_size(Some(STREAM_WINDOW))
                        .initial_connection_window_size(Some(CONNECTION_WINDOW))
                        .adaptive_window(false)
                        .serve_connection(TokioIo::new(stream),service).await;
                });
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {return Err(Error::Unavailable);};
                let Ok(permit) = connections.clone().try_acquire_owned() else {incoming.refuse();continue;};
                let (handler,handlers,budget) = (handler.clone(),handlers.clone(),budget.clone());
                tasks.spawn(async move {
                    let _permit = permit;
                    let Ok(Ok(connection)) = tokio::time::timeout(HANDSHAKE_TIMEOUT,incoming).await else {return;};
                    let negotiated = connection.handshake_data()
                        .and_then(|h| h.downcast::<quinn::crypto::rustls::HandshakeData>().ok());
                    if negotiated.as_ref().and_then(|h| h.protocol.as_deref()) != Some(b"h3") {return;}
                    let Ok(mut connection) = h3::server::builder().max_field_section_size(u64::from(HEADERS))
                        .build(h3_guard::Connection::new(connection)).await else {return;};
                    let mut streams = tokio::task::JoinSet::new();
                    loop {
                        tokio::select! {
                            request = connection.accept() => {
                                let Ok(Some(request)) = request else {break;};
                                let (handler,handlers,budget) = (handler.clone(),handlers.clone(),budget.clone());
                                streams.spawn(async move {
                                    let _ = tokio::time::timeout(RPC_TIMEOUT, async move {
                                    let Ok((request,mut stream)) = request.resolve_request().await else {return;};
                                    if !valid_request(&request) {return;}
                                    let Ok(_permit) = handlers.acquire().await else {return;};
                                    let result = async {
                                        let mut body = Accumulator::default();
                                        while let Some(mut data) = stream.recv_data().await.map_err(|_| Error::Protocol)? {
                                            let bytes = data.copy_to_bytes(data.remaining());
                                            body.push(&bytes,budget.clone()).await?;
                                        }
                                        let reply = dispatch(Protocol::H3,body.finish()?,handler).await?;
                                        let response = http::Response::builder().status(reply.status).header("content-type",CONTENT).body(()).map_err(|_| Error::Protocol)?;
                                        stream.send_response(response).await.map_err(|_| Error::Protocol)?;
                                        stream.send_data(Bytes::from(peer_wire::encode(&reply)?)).await.map_err(|_| Error::Protocol)?;
                                        stream.finish().await.map_err(|_| Error::Protocol)?;
                                        Ok::<_,Error>(())
                                    }.await;
                                    let _ = result;
                                    }).await;
                                });
                            }
                            Some(_) = streams.join_next() => {}
                        }
                    }
                });
            }
            Some(_) = tasks.join_next() => {}
        }
    }
}

#[cfg(test)]
#[path = "peer_http/tests/bench.rs"]
mod bench;
#[cfg(test)]
mod frame_bound_tests;
#[cfg(test)]
#[path = "peer_http/tests/qualification.rs"]
mod qualification;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "peer_http/tests/response_tests.rs"]
mod response_tests;
