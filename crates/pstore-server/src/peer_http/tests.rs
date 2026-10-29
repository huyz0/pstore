//! Real TCP and UDP listeners prove which encrypted HTTP transport ran.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
use super::{Client, Handler, Protocol, serve};
use crate::tls_for_tests as fixtures;
use crate::{
    PeerTls,
    peer_wire::{Reply, Request, VERSION, WIRE_BUDGET},
};
use fixtures::Authority;
use serde_json::json;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

/// A TCP listener and a UDP socket on one free port, as a peer serves both. ⚠️ Binding UDP to
/// whatever port TCP was given raced any other socket holding that number, and the unwrap
/// failed with `AddrInUse` in CI; a port whose UDP half is taken is skipped instead, as
/// `tests/it/peers.rs` already does.
pub(crate) async fn same_port() -> (tokio::net::TcpListener, std::net::UdpSocket) {
    for _ in 0..64 {
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        if let Ok(udp) = std::net::UdpSocket::bind(tcp.local_addr().unwrap()) {
            return (tcp, udp);
        }
    }
    panic!("no port free for both TCP and UDP in 64 tries");
}

#[test]
fn only_quic_availability_failures_allow_transport_retry() {
    let closed = |error_code| {
        quinn::ConnectionError::ConnectionClosed(quinn::ConnectionClose {
            error_code,
            frame_type: None,
            reason: bytes::Bytes::from_static(b"do not inspect untrusted reason strings"),
        })
    };
    for unavailable in [
        quinn::ConnectionError::TimedOut,
        quinn::ConnectionError::LocallyClosed,
        quinn::ConnectionError::Reset,
    ] {
        assert!(matches!(
            super::quic_error(unavailable),
            super::Error::Unavailable
        ));
    }
    for fatal in [
        quinn::ConnectionError::VersionMismatch,
        quinn::ConnectionError::CidsExhausted,
    ] {
        assert!(matches!(
            super::quic_error(fatal),
            super::Error::Authentication
        ));
    }
    for code in [
        quinn::TransportErrorCode::NO_ERROR,
        quinn::TransportErrorCode::INTERNAL_ERROR,
        quinn::TransportErrorCode::CONNECTION_REFUSED,
    ] {
        assert!(matches!(
            super::quic_error(closed(code)),
            super::Error::Unavailable
        ));
    }
    for alert in 0..=u8::MAX {
        assert!(
            matches!(
                super::quic_error(closed(quinn::TransportErrorCode::crypto(alert))),
                super::Error::Authentication
            ),
            "remote TLS alert {alert} must not permit a transport retry"
        );
    }
    for code in [
        quinn::TransportErrorCode::PROTOCOL_VIOLATION,
        quinn::TransportErrorCode::FLOW_CONTROL_ERROR,
        quinn::TransportErrorCode::FRAME_ENCODING_ERROR,
    ] {
        assert!(matches!(
            super::quic_error(closed(code)),
            super::Error::Authentication
        ));
    }
}

#[tokio::test]
async fn accepted_peer_tcp_sockets_disable_nagle_before_tls() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted, client) = tokio::join!(
        super::accept_tcp(&listener),
        tokio::net::TcpStream::connect(address)
    );
    let accepted = accepted.unwrap();
    let client = client.unwrap();
    assert_eq!(accepted.peer_addr().unwrap(), client.local_addr().unwrap());
    assert!(
        accepted.nodelay().unwrap(),
        "peer replies must not wait for a Nagle/delayed-ACK cycle"
    );
}

#[tokio::test]
async fn inbound_and_outbound_connections_share_the_same_admission_ceiling() {
    use super::{Limits, PeerTransport, Pool};
    let ca = Authority::new();
    let limits = Limits::new(2).unwrap();
    let pool = Pool::with_limits(
        tls(&ca, "client"),
        PeerTransport::H3,
        std::time::Duration::from_millis(500),
        limits.clone(),
    )
    .unwrap();
    let (first, remote) = start(tls(&ca, "127.0.0.1")).await;
    pool.call(
        &format!("https://{first}"),
        pstore_types::TenantId(9),
        json!(1),
    )
    .await
    .unwrap();
    let (tcp, udp) = same_port().await;
    let address = tcp.local_addr().unwrap();
    let handler: Handler = Arc::new(|_, req| {
        Box::pin(async move {
            Reply {
                version: VERSION,
                id: req.id,
                status: 200,
                body: req.body,
            }
        })
    });
    let identity = tls(&ca, "127.0.0.1");
    let local = {
        let limits = limits.clone();
        tokio::spawn(async move {
            super::serve_with_limits(tcp, udp, identity, handler, limits)
                .await
                .unwrap()
        })
    };
    let incoming = Client::connect(
        address,
        "127.0.0.1",
        &tls(&ca, "client"),
        Protocol::H2,
        Arc::new(Semaphore::new(WIRE_BUDGET)),
    )
    .await
    .unwrap();
    incoming
        .call(Request {
            version: VERSION,
            id: 1,
            tenant: "9".to_owned(),
            body: json!(2),
        })
        .await
        .unwrap();
    assert_eq!(limits.connections.available_permits(), 0);
    let (second, remote2) = start(tls(&ca, "127.0.0.1")).await;
    let second = format!("https://{second}");
    assert!(
        pool.call(&second, pstore_types::TenantId(9), json!(3))
            .await
            .is_err()
    );
    assert_eq!(pool.stats().connections, 2);
    drop(incoming);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while limits.connections.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        pool.call(&second, pstore_types::TenantId(9), json!(3))
            .await
            .unwrap()
            .body["value"],
        3
    );
    for task in [local, remote, remote2] {
        task.abort();
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct TypedPayload {
    scalars: Vec<f32>,
    codes: Vec<u32>,
    transport: String,
}

#[tokio::test]
async fn typed_payloads_preserve_float_bits_without_a_json_value_bridge() {
    let ca = Authority::new();
    let m = ca.issue(&["localhost", "127.0.0.1"], false);
    let tls = PeerTls::from_pem(m.ca.as_bytes(), m.cert.as_bytes(), m.key.as_bytes()).unwrap();
    let (tcp, udp) = same_port().await;
    let address = tcp.local_addr().unwrap();
    let handler: Handler<TypedPayload, TypedPayload> = Arc::new(|protocol, mut req| {
        Box::pin(async move {
            req.body.transport = format!("{protocol:?}");
            Reply {
                version: VERSION,
                id: req.id,
                status: 200,
                body: req.body,
            }
        })
    });
    let server = tokio::spawn(async move {
        super::serve_typed(tcp, udp, tls, handler).await.unwrap();
    });
    let bits = [0, 0x8000_0000, 0x3f80_0000, 0x7fc0_1234, 0x7f80_0000, 1];
    let payload = || TypedPayload {
        scalars: bits.iter().copied().map(f32::from_bits).collect(),
        codes: vec![0, u32::MAX],
        transport: String::new(),
    };
    for (protocol, mode) in [
        (Protocol::H2, super::PeerTransport::H2),
        (Protocol::H3, super::PeerTransport::H3),
    ] {
        let client_tls = self::tls(&ca, "client");
        let client = Client::connect(
            address,
            "localhost",
            &client_tls,
            protocol,
            Arc::new(Semaphore::new(WIRE_BUDGET)),
        )
        .await
        .unwrap();
        let reply: Reply<TypedPayload> = client
            .call_typed(Request {
                version: VERSION,
                id: 17,
                tenant: "9".to_owned(),
                body: payload(),
            })
            .await
            .unwrap();
        assert_eq!(
            reply
                .body
                .scalars
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            bits
        );
        assert_eq!(reply.body.codes, vec![0, u32::MAX]);
        assert_eq!(reply.body.transport, format!("{protocol:?}"));
        let pool = super::Pool::new(client_tls, mode, std::time::Duration::from_secs(2)).unwrap();
        let reply: Reply<TypedPayload> = pool
            .call_typed(
                &format!("https://{address}"),
                pstore_types::TenantId(9),
                payload(),
            )
            .await
            .unwrap();
        assert_eq!(
            reply
                .body
                .scalars
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            bits
        );
        assert_eq!(reply.body.transport, format!("{protocol:?}"));
    }
    server.abort();
}

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Inspect advertised flow-control credit on the wire, independent of builders.
async fn advertised_h2_windows<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    socket: &mut S,
) -> (u32, u32) {
    let mut stream = None;
    let mut connection = None;
    for _ in 0..8 {
        let mut header = [0; 9];
        socket.read_exact(&mut header).await.unwrap();
        let [a, b, c, kind, flags, d, e, f, g] = header;
        let length = u32::from_be_bytes([0, a, b, c]) as usize;
        assert!(length <= 8192, "unexpected large control frame");
        let mut payload = vec![0; length];
        socket.read_exact(&mut payload).await.unwrap();
        let id = u32::from_be_bytes([d, e, f, g]) & 0x7fff_ffff;
        if kind == 4 && flags & 1 == 0 {
            for setting in payload.chunks_exact(6) {
                let [a, b, c, d, e, f]: [u8; 6] = setting.try_into().unwrap();
                if u16::from_be_bytes([a, b]) == 4 {
                    stream = Some(u32::from_be_bytes([c, d, e, f]));
                }
            }
            socket
                .write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0])
                .await
                .unwrap();
        }
        if kind == 8 && id == 0 {
            let increment = u32::from_be_bytes(payload.try_into().unwrap()) & 0x7fff_ffff;
            connection = Some(65_535 + increment);
        }
        if let (Some(stream), Some(connection)) = (stream, connection) {
            return (stream, connection);
        }
    }
    panic!("peer did not advertise stream and connection credit");
}

#[tokio::test]
async fn http2_advertises_bounded_receive_credit_in_both_directions() {
    let ca = Authority::new();
    let (address, server) = start(tls(&ca, "localhost")).await;
    let (_, client_config) = tls(&ca, "client").configs_for(b"h2").unwrap();
    let socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut socket = tokio_rustls::TlsConnector::from(client_config)
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            socket,
        )
        .await
        .unwrap();
    socket.write_all(H2_PREFACE).await.unwrap();
    socket
        .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
        .await
        .unwrap();
    let windows = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        advertised_h2_windows(&mut socket),
    )
    .await
    .unwrap();
    assert_eq!(windows, (65_536, 262_144), "server receive credit");
    drop(socket);
    server.abort();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client_tls = tls(&ca, "client");
    let client = tokio::spawn(async move {
        Client::connect(
            address,
            "localhost",
            &client_tls,
            Protocol::H2,
            Arc::new(Semaphore::new(WIRE_BUDGET)),
        )
        .await
        .unwrap()
    });
    let (socket, _) = listener.accept().await.unwrap();
    let (server_config, _) = tls(&ca, "localhost").configs_for(b"h2").unwrap();
    let mut socket = tokio_rustls::TlsAcceptor::from(server_config)
        .accept(socket)
        .await
        .unwrap();
    socket
        .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
        .await
        .unwrap();
    let mut preface = [0; 24];
    socket.read_exact(&mut preface).await.unwrap();
    assert_eq!(&preface, H2_PREFACE);
    let windows = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        advertised_h2_windows(&mut socket),
    )
    .await
    .unwrap();
    assert_eq!(windows, (65_536, 262_144), "client receive credit");
    let _client = client.await.unwrap();
}

#[tokio::test]
async fn both_http_versions_transfer_payloads_larger_than_flow_control_windows() {
    let ca = Authority::new();
    let (address, server) = start(tls(&ca, "localhost")).await;
    let value = "payload".repeat(75_000);
    for protocol in [Protocol::H2, Protocol::H3] {
        let client = Client::connect(
            address,
            "localhost",
            &tls(&ca, "client"),
            protocol,
            Arc::new(Semaphore::new(WIRE_BUDGET)),
        )
        .await
        .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.call(Request {
                version: VERSION,
                id: 1,
                tenant: "9".to_owned(),
                body: json!({"large":value}),
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.body["value"]["large"], value);
    }
    server.abort();
}

/// Bypass the application encoder to probe the transport's refusal boundary.
async fn raw_call(client: &Client, body: bytes::Bytes) -> Result<u16, ()> {
    let uri = format!("https://{}/peer/v1/part", client.authority);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(uri.as_str())
        .header("content-type", "application/msgpack");
    raw_request(client, body, request).await
}

async fn raw_request(
    client: &Client,
    body: bytes::Bytes,
    request: axum::http::request::Builder,
) -> Result<u16, ()> {
    match &client.sender {
        super::Sender::H2(sender) => {
            let mut sender = sender.clone();
            sender.ready().await.map_err(|_| ())?;
            let reply = sender
                .send_request(request.body(http_body_util::Full::new(body)).unwrap())
                .await
                .map_err(|_| ())?;
            Ok(reply.status().as_u16())
        }
        super::Sender::H3(sender) => {
            let mut sender = sender.clone();
            let mut stream = sender
                .send_request(request.body(()).unwrap())
                .await
                .map_err(|_| ())?;
            stream.send_data(body).await.map_err(|_| ())?;
            stream.finish().await.map_err(|_| ())?;
            Ok(stream
                .recv_response()
                .await
                .map_err(|_| ())?
                .status()
                .as_u16())
        }
    }
}

fn tls(ca: &Authority, name: &str) -> PeerTls {
    let m = ca.issue(&[name], false);
    PeerTls::from_pem(m.ca.as_bytes(), m.cert.as_bytes(), m.key.as_bytes()).unwrap()
}

#[tokio::test]
async fn both_http_versions_refuse_wrong_route_content_and_bearer_headers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let ca = Authority::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let handler: Handler = {
        let calls = calls.clone();
        Arc::new(move |_, req| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Reply {
                    version: VERSION,
                    id: req.id,
                    status: 200,
                    body: req.body,
                }
            })
        })
    };
    let (address, server) = start_with(tls(&ca, "localhost"), handler).await;
    let body = crate::peer_wire::encode(&Request {
        version: VERSION,
        id: 7,
        tenant: "9".to_owned(),
        body: json!({}),
    })
    .unwrap();
    for protocol in [Protocol::H2, Protocol::H3] {
        for (method, path, content, bearer) in [
            ("GET", "/peer/v1/part", "application/msgpack", false),
            ("POST", "/v1/internal/part", "application/msgpack", false),
            (
                "POST",
                "/peer/v1/part?tenant=10",
                "application/msgpack",
                false,
            ),
            ("POST", "/peer/v1/part", "application/json", false),
            ("POST", "/peer/v1/part", "application/msgpack", true),
        ] {
            let client = Client::connect(
                address,
                "localhost",
                &tls(&ca, "client"),
                protocol,
                Arc::new(Semaphore::new(WIRE_BUDGET)),
            )
            .await
            .unwrap();
            let mut request = axum::http::Request::builder()
                .method(method)
                .uri(format!("https://{}{path}", client.authority))
                .header("content-type", content);
            if bearer {
                request = request.header("authorization", "Bearer a-public-token");
            }
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                raw_request(&client, bytes::Bytes::copy_from_slice(&body), request),
            )
            .await;
            assert!(matches!(result, Ok(Err(_)) | Ok(Ok(400..=599))));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }
    server.abort();
}

#[tokio::test]
async fn both_http_versions_refuse_mismatched_response_identity_and_version() {
    let ca = Authority::new();
    let handler: Handler = Arc::new(|_, req| {
        Box::pin(async move {
            Reply {
                version: if req.id == 1 { VERSION + 1 } else { VERSION },
                id: if req.id == 2 { req.id + 1 } else { req.id },
                status: 200,
                body: json!({}),
            }
        })
    });
    let (address, server) = start_with(tls(&ca, "localhost"), handler).await;
    for protocol in [Protocol::H2, Protocol::H3] {
        let client = Client::connect(
            address,
            "localhost",
            &tls(&ca, "client"),
            protocol,
            Arc::new(Semaphore::new(WIRE_BUDGET)),
        )
        .await
        .unwrap();
        for id in [1, 2] {
            let result = client
                .call(Request {
                    version: VERSION,
                    id,
                    tenant: "9".to_owned(),
                    body: json!({}),
                })
                .await;
            assert!(matches!(result, Err(super::Error::Protocol)));
        }
    }
    server.abort();
}

async fn start(server: PeerTls) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let handler: Handler = Arc::new(|protocol, req| {
        Box::pin(async move {
            Reply {
                version: VERSION,
                id: req.id,
                status: 200,
                body: json!({"transport":format!("{protocol:?}"),"tenant":req.tenant,"value":req.body}),
            }
        })
    });
    start_with(server, handler).await
}

async fn start_with(
    server: PeerTls,
    handler: Handler,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let (tcp, udp) = same_port().await;
    let address = tcp.local_addr().unwrap();
    let task = tokio::spawn(async move {
        serve(tcp, udp, server, handler).await.unwrap();
    });
    (address, task)
}

#[tokio::test]
async fn both_http_versions_run_messagepack_over_verified_mutual_tls() {
    let ca = Authority::new();
    let (address, server) = start(tls(&ca, "localhost")).await;
    for protocol in [Protocol::H2, Protocol::H3] {
        let client = Client::connect(
            address,
            "localhost",
            &tls(&ca, "client"),
            protocol,
            Arc::new(Semaphore::new(WIRE_BUDGET)),
        )
        .await
        .unwrap();
        let reply = client
            .call(Request {
                version: VERSION,
                id: 7,
                tenant: u128::MAX.to_string(),
                body: json!({"phase":"open","bits":[0,u32::MAX]}),
            })
            .await
            .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.id, 7);
        assert_eq!(reply.body["transport"], format!("{protocol:?}"));
        assert_eq!(reply.body["tenant"], u128::MAX.to_string());
        assert_eq!(reply.body["value"]["bits"], json!([0, u32::MAX]));
    }
    server.abort();
}

#[tokio::test]
async fn both_http_versions_refuse_wrong_server_name_and_untrusted_client() {
    let ca = Authority::new();
    let (address, server) = start(tls(&ca, "localhost")).await;
    for protocol in [Protocol::H2, Protocol::H3] {
        let alien = Authority::new().issue(&["stranger"], false);
        let untrusted_client = PeerTls::from_pem(
            ca.pem.as_bytes(),
            alien.cert.as_bytes(),
            alien.key.as_bytes(),
        )
        .unwrap();
        for (name, identity) in [
            ("wrong.example", tls(&ca, "client")),
            ("localhost", untrusted_client),
            ("localhost", {
                let expired = ca.issue(&["client"], true);
                PeerTls::from_pem(
                    expired.ca.as_bytes(),
                    expired.cert.as_bytes(),
                    expired.key.as_bytes(),
                )
                .unwrap()
            }),
        ] {
            let result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                let client = Client::connect(
                    address,
                    name,
                    &identity,
                    protocol,
                    Arc::new(Semaphore::new(WIRE_BUDGET)),
                )
                .await?;
                client
                    .call(Request {
                        version: VERSION,
                        id: 1,
                        tenant: "9".to_owned(),
                        body: json!({}),
                    })
                    .await
            })
            .await;
            assert!(
                matches!(result, Ok(Err(_))),
                "certificate refusal must finish with an error"
            );
        }
    }
    server.abort();
}

#[tokio::test]
async fn both_http_versions_refuse_invalid_frames_before_dispatch() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let ca = Authority::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let handler: Handler = {
        let calls = calls.clone();
        Arc::new(move |_, req| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Reply {
                    version: VERSION,
                    id: req.id,
                    status: 200,
                    body: req.body,
                }
            })
        })
    };
    let (address, server) = start_with(tls(&ca, "localhost"), handler).await;
    let malformed = [
        vec![0, 0],
        vec![0, 0, 0, 0],
        ((crate::peer_wire::MAX_FRAME + 1) as u32)
            .to_be_bytes()
            .to_vec(),
        vec![0, 0, 0, 8, 1],
        vec![0, 0, 0, 1, 0xc1],
        vec![0, 0, 0, 2, 1, 2],
        crate::peer_wire::encode(&Request {
            version: VERSION + 1,
            id: 1,
            tenant: "9".to_owned(),
            body: json!({}),
        })
        .unwrap(),
        crate::peer_wire::encode(&Request {
            version: VERSION,
            id: 0,
            tenant: "9".to_owned(),
            body: json!({}),
        })
        .unwrap(),
        crate::peer_wire::encode(&Request {
            version: VERSION,
            id: 1,
            tenant: "-1".to_owned(),
            body: json!({}),
        })
        .unwrap(),
    ];
    for protocol in [Protocol::H2, Protocol::H3] {
        for wire in &malformed {
            // A refused stream may close its connection; each independent probe
            // authenticates afresh so one refusal cannot hide another.
            let client = Client::connect(
                address,
                "localhost",
                &tls(&ca, "client"),
                protocol,
                Arc::new(Semaphore::new(WIRE_BUDGET)),
            )
            .await
            .unwrap();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                raw_call(&client, bytes::Bytes::copy_from_slice(wire)),
            )
            .await;
            assert!(
                matches!(result, Ok(Err(_)) | Ok(Ok(400..=599))),
                "malformed frame must be refused promptly"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "malformed frame reached handler"
            );
        }
    }
    server.abort();
}

#[tokio::test]
async fn both_http_versions_multiplex_independent_calls_and_match_reply_ids() {
    let ca = Authority::new();
    let released = Arc::new(tokio::sync::Notify::new());
    let handler: Handler = Arc::new(move |_, req| {
        let released = released.clone();
        Box::pin(async move {
            if req.id == 1 {
                released.notified().await;
            } else {
                released.notify_one();
            }
            Reply {
                version: VERSION,
                id: req.id,
                status: 200,
                body: req.body,
            }
        })
    });
    let (address, server) = start_with(tls(&ca, "localhost"), handler).await;
    for protocol in [Protocol::H2, Protocol::H3] {
        let client = Client::connect(
            address,
            "localhost",
            &tls(&ca, "client"),
            protocol,
            Arc::new(Semaphore::new(WIRE_BUDGET)),
        )
        .await
        .unwrap();
        let req = |id| Request {
            version: VERSION,
            id,
            tenant: "9".to_owned(),
            body: json!({"id":id}),
        };
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(client.call(req(1)), client.call(req(2)))
        })
        .await
        .expect("one blocked RPC must not block a second stream");
        assert_eq!(first.unwrap().body, json!({"id":1}));
        assert_eq!(second.unwrap().body, json!({"id":2}));
    }
    server.abort();
}

#[test]
fn peer_metadata_requires_the_exact_route_and_no_bearer_header() {
    for (method, path, content, bearer, allowed) in [
        (
            "POST",
            "/peer/v1/part",
            Some("application/msgpack"),
            false,
            true,
        ),
        (
            "GET",
            "/peer/v1/part",
            Some("application/msgpack"),
            false,
            false,
        ),
        (
            "POST",
            "/peer/v1/part/",
            Some("application/msgpack"),
            false,
            false,
        ),
        (
            "POST",
            "/peer/v1/part?tenant=9",
            Some("application/msgpack"),
            false,
            false,
        ),
        ("POST", "/peer/v1/part", None, false, false),
        (
            "POST",
            "/peer/v1/part",
            Some("application/json"),
            false,
            false,
        ),
        (
            "POST",
            "/peer/v1/part",
            Some("application/msgpack"),
            true,
            false,
        ),
    ] {
        let mut builder = axum::http::Request::builder().method(method).uri(path);
        if let Some(content) = content {
            builder = builder.header("content-type", content);
        }
        if bearer {
            builder = builder.header("authorization", "");
        }
        assert_eq!(super::valid_request(&builder.body(()).unwrap()), allowed);
    }
}

#[tokio::test]
async fn peer_envelopes_refuse_invalid_identity_before_dispatch() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let invoked = Arc::new(AtomicUsize::new(0));
    let handler: Handler = {
        let invoked = invoked.clone();
        Arc::new(move |_, request| {
            invoked.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Reply {
                    version: VERSION,
                    id: request.id,
                    status: 200,
                    body: request.body,
                }
            })
        })
    };
    for protocol in [Protocol::H2, Protocol::H3] {
        for (version, id, tenant, allowed) in [
            (VERSION, 1, "9".to_owned(), true),
            (0, 1, "9".to_owned(), false),
            (VERSION, 0, "9".to_owned(), false),
            (VERSION, 1, String::new(), false),
            (VERSION, 1, "not-decimal".to_owned(), false),
            (VERSION, 1, format!("{}0", u128::MAX), false),
            (VERSION, 1, u128::MAX.to_string(), true),
        ] {
            let budget = Arc::new(Semaphore::new(WIRE_BUDGET));
            let bytes = crate::peer_wire::encode(&Request {
                version,
                id,
                tenant,
                body: json!("identity"),
            })
            .unwrap();
            let mut accumulator = crate::peer_wire::Accumulator::default();
            accumulator.push(&bytes, budget.clone()).await.unwrap();
            let before = invoked.load(Ordering::SeqCst);
            let result =
                super::dispatch(protocol, accumulator.finish().unwrap(), handler.clone()).await;
            assert_eq!(result.is_ok(), allowed);
            assert_eq!(
                invoked.load(Ordering::SeqCst),
                before + usize::from(allowed)
            );
            assert_eq!(budget.available_permits(), WIRE_BUDGET);
        }
    }
    assert!(
        super::response(&Reply {
            version: VERSION,
            id: 1,
            status: 0,
            body: json!(null)
        })
        .is_err()
    );
    let encoded = super::response(&Reply {
        version: VERSION,
        id: 1,
        status: 200,
        body: json!("valid"),
    })
    .unwrap();
    assert_eq!(encoded.status(), 200);
}
