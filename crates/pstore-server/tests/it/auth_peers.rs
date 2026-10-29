//! Actual vector and phased text execution over both mutually authenticated transports.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions and fixtures report failure by panic"
)]
use crate::support;
use crate::tls_fixture;
use axum::http::StatusCode;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::{Api, PeerConfig, PeerTls, PeerTransport};
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use support::auth::*;
use tls_fixture::Authority;

fn identity(ca: &Authority) -> PeerTls {
    let material = ca.issue(&["127.0.0.1"], false);
    PeerTls::from_pem(
        material.ca.as_bytes(),
        material.cert.as_bytes(),
        material.key.as_bytes(),
    )
    .unwrap()
}

async fn start(api: Arc<Api<MemoryStore>>) -> (String, tokio::task::JoinHandle<()>) {
    // ⚠️ A port whose UDP half another socket holds is skipped, not unwrapped (`AddrInUse`
    // failed CI), as `peers.rs` does.
    let (tcp, udp) = 'bound: {
        for _ in 0..64 {
            let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            if let Ok(udp) = std::net::UdpSocket::bind(tcp.local_addr().unwrap()) {
                break 'bound (tcp, udp);
            }
        }
        panic!("no port free for both TCP and UDP in 64 tries");
    };
    let address = tcp.local_addr().unwrap();
    let task = tokio::spawn(async move {
        pstore_server::serve_peers(api, tcp, udp, std::future::pending())
            .await
            .unwrap();
    });
    (format!("https://{address}"), task)
}

async fn metric(api: &Arc<Api<MemoryStore>>, prefix: &str) -> u64 {
    let got = send(
        api,
        "GET",
        "/metrics",
        Some(&token("9", &["admin"])),
        None,
        &Value::Null,
    )
    .await;
    assert_eq!(got.status, StatusCode::OK);
    got.text
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("{prefix} not in metrics"))
        .parse()
        .unwrap()
}

async fn completed(api: &Arc<Api<MemoryStore>>, mode: PeerTransport, phase: &str) -> u64 {
    let transport = match mode {
        PeerTransport::H2 => "h2",
        PeerTransport::H3 => "h3",
        PeerTransport::Auto => panic!("explicit transport required"),
    };
    metric(
        api,
        &format!(
            "pstore_peer_parts_completed_total{{transport=\"{transport}\",phase=\"{phase}\"}} "
        ),
    )
    .await
}

#[tokio::test]
async fn authenticated_vector_and_phased_text_parts_match_local_execution() {
    for mode in [PeerTransport::H2, PeerTransport::H3] {
        let cfg = config(&[("current", &KEY)], "current");
        let inner = MemoryStore::new();
        let store = Accounted::new(inner.clone());
        let ca = Authority::new();
        let local = Api::with_auth(
            store.clone(),
            LaneId(1),
            auth(&cfg).with_peer_tls(identity(&ca), mode),
        )
        .unwrap();
        let peer = Api::with_auth(
            store.clone(),
            LaneId(2),
            auth(&cfg).with_peer_tls(identity(&ca), mode),
        )
        .unwrap();
        for api in [&local, &peer] {
            api.set_index_params_for_test(pstore_index::cluster::Params {
                exact_scan_threshold: 8,
                ..Default::default()
            });
        }
        let (url, serving) = start(peer.clone()).await;
        let urls = vec!["https://127.0.0.1:1".to_owned(), url];
        let writer = token("9", &["write", "admin"]);
        let mut distributed = false;
        for batch in 0..40 {
            let documents: Vec<Value> = (batch * 24..(batch + 1) * 24)
                .map(|i| {
                    json!({
                        "id":format!("d{i:04}"),"vector":[(i as f32).sin(),1.0],
                        "text":format!("common word{}",i%5),"attributes":{"n":i}
                    })
                })
                .collect();
            let got = send(
                &local,
                "PUT",
                "/v1/indexes/docs/documents",
                Some(&writer),
                None,
                &json!({"durability":"durable","documents":documents}),
            )
            .await;
            assert_eq!(got.status, StatusCode::OK, "{}", got.text);
            let got = send(
                &local,
                "POST",
                "/v1/admin/fold",
                Some(&writer),
                None,
                &Value::Null,
            )
            .await;
            assert_eq!(got.status, StatusCode::OK, "{}", got.text);
            if batch >= 3 {
                let head = local
                    .hold_engine_for_test(TenantId(9))
                    .await
                    .head_for_test()
                    .await;
                distributed = head
                    .indexes
                    .get("docs")
                    .unwrap()
                    .iter()
                    .any(|segment| pstore_engine::assign(&segment.key, &urls) == 1);
                if distributed {
                    break;
                }
            }
        }
        assert!(distributed, "fixture must give the remote peer actual work");
        let reader = token("9", &["read"]);
        let denied = send(
            &peer,
            "POST",
            "/v1/internal/part",
            Some(&reader),
            None,
            &json!({}),
        )
        .await;
        assert_eq!(denied.status, StatusCode::NOT_FOUND);
        let queries = [
            json!({"vector":[0.3,1.0],"top_k":10,"include_attributes":true}),
            json!({"text":"common word2","top_k":10,"include_attributes":true}),
        ];
        for query in &queries {
            local.set_peers(None).unwrap();
            let expected = send(
                &local,
                "POST",
                "/v1/indexes/docs/query",
                Some(&reader),
                None,
                query,
            )
            .await;
            assert_eq!(expected.status, StatusCode::OK, "{}", expected.text);
            local
                .set_peers(Some(PeerConfig {
                    servers: urls.clone(),
                    me: 0,
                    timeout: Duration::from_secs(2),
                }))
                .unwrap();
            let phase = if query.get("text").is_some() {
                "scan"
            } else {
                "whole"
            };
            let before = completed(&peer, mode, phase).await;
            let opened = completed(&peer, mode, "open").await;
            let failures = metric(&local, "pstore_peer_parts_failed ").await;
            let actual = send(
                &local,
                "POST",
                "/v1/indexes/docs/query",
                Some(&reader),
                None,
                query,
            )
            .await;
            assert_eq!(actual.status, StatusCode::OK, "{}", actual.text);
            assert_eq!(actual.body["results"], expected.body["results"]);
            assert_eq!(actual.body["meta"]["epoch"], expected.body["meta"]["epoch"]);
            assert!(
                completed(&peer, mode, phase).await > before,
                "remote phase did not complete"
            );
            assert_eq!(
                metric(&local, "pstore_peer_parts_failed ").await,
                failures,
                "healthy peer fell back"
            );
            if phase == "scan" {
                assert!(
                    completed(&peer, mode, "open").await > opened,
                    "remote open did not complete"
                );
                assert_eq!(
                    metric(&peer, "pstore_peer_parts_held ").await,
                    0,
                    "scan did not consume its opened state"
                );
            }
        }
        serving.abort();
        // A different peer CA fails authentication and preserves the local answer.
        let stranger = Authority::new();
        let wrong = Api::with_auth(
            store,
            LaneId(3),
            auth(&cfg).with_peer_tls(identity(&stranger), mode),
        )
        .unwrap();
        let (url, serving) = start(wrong.clone()).await;
        local.set_peers(None).unwrap();
        let head = local
            .hold_engine_for_test(TenantId(9))
            .await
            .head_for_test()
            .await;
        let wrong_urls = (1000..2000)
            .find_map(|port| {
                let candidates = vec![format!("https://127.0.0.1:{port}"), url.clone()];
                head.indexes
                    .get("docs")
                    .unwrap()
                    .iter()
                    .any(|segment| pstore_engine::assign(&segment.key, &candidates) == 1)
                    .then_some(candidates)
            })
            .expect("wrong CA fixture must assign a current segment to its remote endpoint");
        let expected = send(
            &local,
            "POST",
            "/v1/indexes/docs/query",
            Some(&reader),
            None,
            &queries[0],
        )
        .await;
        local
            .set_peers(Some(PeerConfig {
                servers: wrong_urls,
                me: 0,
                timeout: Duration::from_secs(2),
            }))
            .unwrap();
        let failures = metric(&local, "pstore_peer_parts_failed ").await;
        let actual = send(
            &local,
            "POST",
            "/v1/indexes/docs/query",
            Some(&reader),
            None,
            &queries[0],
        )
        .await;
        assert_eq!(actual.status, StatusCode::OK, "{}", actual.text);
        assert_eq!(actual.body["results"], expected.body["results"]);
        assert!(
            metric(&local, "pstore_peer_parts_failed ").await > failures,
            "wrong CA control did not attempt a peer"
        );
        assert_eq!(completed(&wrong, mode, "whole").await, 0);
        serving.abort();
    }
}
