//! Encrypted MessagePack adapters for legacy search/fault fixtures only.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "shared test fixtures use subsets per target and report failures through assertions"
)]
#[path = "tls.rs"]
mod certificates;
use axum::body::Body;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};
use pstore_server::{Auth, PeerTls, PeerTransport};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};
use tower::ServiceExt;

struct Identity {
    peer: PeerTls,
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
    material: certificates::Material,
}

fn identity() -> &'static Identity {
    static IDENTITY: OnceLock<Identity> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        let ca = certificates::Authority::new();
        let material = ca.issue(&["localhost", "127.0.0.1"], false);
        let peer = PeerTls::from_pem(
            material.ca.as_bytes(),
            material.cert.as_bytes(),
            material.key.as_bytes(),
        )
        .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        for certificate in CertificateDer::pem_reader_iter(material.ca.as_bytes()) {
            roots.add(certificate.unwrap()).unwrap();
        }
        let certificates = CertificateDer::pem_reader_iter(material.cert.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_reader(material.key.as_bytes()).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots.clone()),
            provider.clone(),
        )
        .build()
        .unwrap();
        let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certificates.clone(), key.clone_key())
            .unwrap();
        let mut client = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(certificates, key)
            .unwrap();
        server.alpn_protocols = vec![b"h2".to_vec()];
        client.alpn_protocols = vec![b"h2".to_vec()];
        server.max_early_data_size = 0;
        client.enable_early_data = false;
        Identity {
            peer,
            client: Arc::new(client),
            server: Arc::new(server),
            material,
        }
    })
}

pub(crate) fn write_identity(directory: &std::path::Path) -> [std::path::PathBuf; 3] {
    let material = &identity().material;
    std::array::from_fn(|i| {
        let (name, content) = [
            ("ca.pem", &material.ca),
            ("cert.pem", &material.cert),
            ("key.pem", &material.key),
        ][i];
        let path = directory.join(name);
        std::fs::write(&path, content).unwrap();
        path
    })
}

/// The standalone process uses the same trusted identity on its H3 listener.
pub(crate) async fn call_h3(url: &str, tenant: &str, part: Value) -> (u16, Value) {
    use bytes::Buf;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let address = url.strip_prefix("https://").unwrap().parse().unwrap();
        let mut config = (*identity().client).clone();
        config.alpn_protocols = vec![b"h3".to_vec()];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(config).unwrap();
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
        let connection = endpoint
            .connect(address, "127.0.0.1")
            .unwrap()
            .await
            .unwrap();
        let negotiated = connection
            .handshake_data()
            .unwrap()
            .downcast::<quinn::crypto::rustls::HandshakeData>()
            .unwrap();
        assert_eq!(negotiated.protocol.as_deref(), Some(b"h3".as_slice()));
        let (mut driver, mut sender) = h3::client::builder()
            .build(h3_quinn::Connection::new(connection))
            .await
            .unwrap();
        struct Driver(tokio::task::JoinHandle<()>);
        impl Drop for Driver {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _driver = Driver(tokio::spawn(async move {
            let _ = futures_util::future::poll_fn(|cx| driver.poll_close(cx)).await;
        }));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(format!("{url}/peer/v1/part"))
            .header("content-type", "application/msgpack")
            .body(())
            .unwrap();
        let mut stream = sender.send_request(request).await.unwrap();
        stream
            .send_data(frame(
                &json!({"version":1,"id":1,"tenant":tenant,"body":{"Whole":part}}),
            ))
            .await
            .unwrap();
        stream.finish().await.unwrap();
        let response = stream.recv_response().await.unwrap();
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await.unwrap() {
            bytes.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
        }
        let reply = decode(&bytes);
        assert_eq!(reply["version"], 1);
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["status"], status);
        (status, reply["body"]["Hits"].clone())
    })
    .await
    .unwrap()
}

pub(crate) fn auth() -> Auth {
    Auth::from_vars(|key| (key == "PSTORE_ALLOW_UNAUTHENTICATED").then(|| "1".to_owned()))
        .unwrap()
        .with_peer_tls(identity().peer.clone(), PeerTransport::H2)
}

fn frame(value: &Value) -> Bytes {
    let encoded = rmp_serde::to_vec_named(value).unwrap();
    let mut result = Vec::with_capacity(encoded.len() + 4);
    result.extend_from_slice(&u32::try_from(encoded.len()).unwrap().to_be_bytes());
    result.extend_from_slice(&encoded);
    Bytes::from(result)
}

fn decode(bytes: &[u8]) -> Value {
    let size = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    assert_eq!(size, bytes.len() - 4);
    rmp_serde::from_slice(&bytes[4..]).unwrap()
}

/// One real mTLS H2 request, returning the legacy fixture's payload shape.
pub(crate) async fn call(url: &str, tenant: &str, part: Value) -> (u16, Value) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let address = url.strip_prefix("https://").unwrap();
        let socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let stream = tokio_rustls::TlsConnector::from(identity().client.clone())
            .connect(
                rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap(),
                socket,
            )
            .await
            .unwrap();
        assert_eq!(stream.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
        let (mut sender, driver) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake(TokioIo::new(stream))
            .await
            .unwrap();
        struct Driver(tokio::task::JoinHandle<()>);
        impl Drop for Driver {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _driver = Driver(tokio::spawn(async move {
            let _ = driver.await;
        }));
        let command = match part["phase"].as_str() {
            Some("open") => "Open",
            Some("scan") => "Scan",
            _ => "Whole",
        };
        let body = frame(&json!({"version":1,"id":1,"tenant":tenant,"body":{(command):part}}));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(format!("{url}/peer/v1/part"))
            .header("content-type", "application/msgpack")
            .body(Full::new(body))
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let reply = decode(&bytes);
        assert_eq!(reply["version"], 1);
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["status"], status);
        let body = &reply["body"];
        let payload = if let Some(error) = body.get("Error") {
            json!({"error":error})
        } else {
            body.as_object().unwrap().values().next().unwrap().clone()
        };
        (status, payload)
    })
    .await
    .unwrap()
}

/// Adapt fault routers while retaining authenticated transport and framed bytes.
pub(crate) async fn serve_fake(listener: tokio::net::TcpListener, router: axum::Router) {
    loop {
        let (socket, _) = listener.accept().await.unwrap();
        let router = router.clone();
        tokio::spawn(async move {
            let stream = tokio_rustls::TlsAcceptor::from(identity().server.clone())
                .accept(socket)
                .await
                .unwrap();
            let service = hyper::service::service_fn(
                move |request: axum::http::Request<hyper::body::Incoming>| {
                    let router = router.clone();
                    async move {
                        let bytes = request.into_body().collect().await.unwrap().to_bytes();
                        let envelope = decode(&bytes);
                        let command = envelope["body"].as_object().unwrap().iter().next().unwrap();
                        let request = axum::http::Request::builder()
                            .method("POST")
                            .uri("/peer/v1/part")
                            .header("content-type", "application/json")
                            .header("x-pstore-tenant", envelope["tenant"].as_str().unwrap())
                            .body(Body::from(serde_json::to_vec(command.1).unwrap()))
                            .unwrap();
                        let response = router.oneshot(request).await.unwrap();
                        let status = response.status();
                        let bytes = response.into_body().collect().await.unwrap().to_bytes();
                        let body = if status.is_success() {
                            let kind = if command.0 == "Open" {
                                "Opened"
                            } else {
                                "Hits"
                            };
                            json!({(kind): serde_json::from_slice::<Value>(&bytes).unwrap()})
                        } else {
                            let parsed =
                                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
                            let error = parsed.get("error").cloned().unwrap_or_else(|| json!({"code":"peer_error","message":String::from_utf8_lossy(&bytes),"retryable":status.is_server_error()}));
                            json!({"Error":error})
                        };
                        let response = axum::http::Response::builder().status(status).header("content-type","application/msgpack").body(Full::new(frame(&json!({"version":1,"id":envelope["id"],"status":status.as_u16(),"body":body})))).unwrap();
                        Ok::<_, std::convert::Infallible>(response)
                    }
                },
            );
            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}
