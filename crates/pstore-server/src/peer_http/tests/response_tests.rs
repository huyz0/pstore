#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "raw peer fixture failures report through assertions"
)]
use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn reply(case: usize) -> (u16, &'static str, Bytes) {
    let valid = peer_wire::encode(&Reply {
        version: peer_wire::VERSION,
        id: 123,
        status: 200,
        body: json!("result"),
    })
    .unwrap();
    let data = match case {
        2 => vec![0, 0, 0, 1, 0xc1],
        3 => vec![0, 0, 0, 10, 0],
        4 => {
            let mut extra = valid;
            extra.push(0);
            extra
        }
        6 => Vec::new(),
        _ => valid,
    };
    (
        if case == 5 { 201 } else { 200 },
        if case == 1 {
            "application/json"
        } else {
            CONTENT
        },
        data.into(),
    )
}

async fn raw_server(
    protocol: Protocol,
) -> (
    SocketAddr,
    PeerTls,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let ca = crate::tls_for_tests::Authority::new();
    let material = ca.issue(&["::1"], false);
    let tls = PeerTls::from_pem(
        material.ca.as_bytes(),
        material.cert.as_bytes(),
        material.key.as_bytes(),
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    match protocol {
        Protocol::H2 => {
            let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (config, _) = tls.configs_for(b"h2").unwrap();
            let task = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let stream = tokio_rustls::TlsAcceptor::from(config)
                    .accept(tcp)
                    .await
                    .unwrap();
                let service =
                    hyper::service::service_fn(move |req: http::Request<hyper::body::Incoming>| {
                        let count = count.clone();
                        async move {
                            req.into_body().collect().await.unwrap();
                            let case = count.fetch_add(1, Ordering::SeqCst);
                            let (status, content, data) = reply(case);
                            let mut frames = vec![Ok::<_, std::convert::Infallible>(
                                hyper::body::Frame::data(data),
                            )];
                            if case == 7 {
                                let mut trailers = http::HeaderMap::new();
                                trailers.insert(
                                    "x-peer-extra",
                                    http::HeaderValue::from_static("refuse"),
                                );
                                frames.push(Ok(hyper::body::Frame::trailers(trailers)));
                            }
                            Ok::<_, std::convert::Infallible>(
                                http::Response::builder()
                                    .status(status)
                                    .header("content-type", content)
                                    .body(http_body_util::StreamBody::new(
                                        futures_util::stream::iter(frames),
                                    ))
                                    .unwrap(),
                            )
                        }
                    });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
            (address, tls, calls, task)
        }
        Protocol::H3 => {
            let (config, _) = tls.configs_for(b"h3").unwrap();
            let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(config).unwrap();
            let endpoint = quinn::Endpoint::server(
                quinn::ServerConfig::with_crypto(Arc::new(crypto)),
                "[::1]:0".parse().unwrap(),
            )
            .unwrap();
            let address = endpoint.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let connection = endpoint.accept().await.unwrap().await.unwrap();
                let mut connection = h3::server::builder()
                    .build(h3_quinn::Connection::new(connection))
                    .await
                    .unwrap();
                while let Ok(Some(request)) = connection.accept().await {
                    let (_, mut stream) = request.resolve_request().await.unwrap();
                    while stream.recv_data().await.unwrap().is_some() {}
                    let case = count.fetch_add(1, Ordering::SeqCst);
                    let (status, content, data) = reply(case);
                    stream
                        .send_response(
                            http::Response::builder()
                                .status(status)
                                .header("content-type", content)
                                .body(())
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    if !data.is_empty() {
                        let _ = stream.send_data(data).await;
                    }
                    let _ = stream.finish().await;
                }
            });
            (address, tls, calls, task)
        }
    }
}

#[tokio::test]
async fn ipv6_clients_refuse_malformed_replies_and_release_wire_capacity() {
    for protocol in [Protocol::H2, Protocol::H3] {
        let (address, tls, calls, task) = raw_server(protocol).await;
        let budget = Arc::new(Semaphore::new(peer_wire::WIRE_BUDGET));
        let client = Client::connect(address, "::1", &tls, protocol, budget.clone())
            .await
            .unwrap();
        assert_eq!(client.authority, format!("[::1]:{}", address.port()));
        let last = if protocol == Protocol::H2 { 7 } else { 6 };
        for case in 0..=last {
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                client.call(Request {
                    version: peer_wire::VERSION,
                    id: 123,
                    tenant: "9".to_owned(),
                    body: json!("request"),
                }),
            )
            .await
            .expect("malformed peer responses must terminate");
            if case == 0 {
                assert_eq!(result.unwrap().body, json!("result"));
            } else if matches!(case, 1 | 5 | 7) {
                assert!(
                    matches!(&result, Err(Error::Protocol)),
                    "case={case}, error={:?}",
                    result.as_ref().err()
                );
            } else {
                assert!(
                    matches!(&result, Err(Error::Wire(_))),
                    "case={case}, error={:?}",
                    result.as_ref().err()
                );
            }
            assert_eq!(budget.available_permits(), peer_wire::WIRE_BUDGET);
            assert_eq!(client.slots.available_permits(), 64);
        }
        assert_eq!(calls.load(Ordering::SeqCst), last + 1);
        client.slots.close();
        assert!(matches!(
            client
                .call(Request {
                    version: peer_wire::VERSION,
                    id: 123,
                    tenant: "9".to_owned(),
                    body: json!(null)
                })
                .await,
            Err(Error::Unavailable)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), last + 1);
        task.abort();
    }
}

struct Unencodable;
impl Serialize for Unencodable {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("deliberate fixture error"))
    }
}

#[tokio::test]
async fn serialization_failures_never_send_partial_requests_or_responses() {
    for protocol in [Protocol::H2, Protocol::H3] {
        let (address, tls, calls, task) = raw_server(protocol).await;
        let client = Client::connect(
            address,
            "::1",
            &tls,
            protocol,
            Arc::new(Semaphore::new(peer_wire::WIRE_BUDGET)),
        )
        .await
        .unwrap();
        let result: Result<Reply, Error> = client
            .call_typed(Request {
                version: peer_wire::VERSION,
                id: 123,
                tenant: "9".to_owned(),
                body: Unencodable,
            })
            .await;
        assert!(matches!(result, Err(Error::Wire(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(client.slots.available_permits(), 64);
        task.abort();
    }
    assert!(matches!(
        response(&Reply {
            version: peer_wire::VERSION,
            id: 1,
            status: 200,
            body: Unencodable
        }),
        Err(Error::Wire(_))
    ));
}
