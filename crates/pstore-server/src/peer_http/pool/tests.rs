#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
use super::Pool;
use crate::tls_for_tests as fixtures;
use crate::{
    PeerTls,
    peer_http::{Handler, PeerTransport, serve},
    peer_wire::{Reply, VERSION},
};
use fixtures::Authority;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn udp_backoff_survives_replacing_a_rejected_http2_connection() {
    use crate::peer_http::Error;
    let ca = Authority::new();
    let (tcp, _blackhole) = crate::peer_http::tests::same_port().await;
    let address = tcp.local_addr().unwrap();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let handler: Handler = Arc::new(|_, req| {
        Box::pin(async move {
            Reply {
                version: VERSION,
                id: if req.body == json!("reject") {
                    0
                } else {
                    req.id
                },
                status: 200,
                body: req.body,
            }
        })
    });
    let tls = identity(&ca, "127.0.0.1");
    let server = tokio::spawn(async move { serve(tcp, udp, tls, handler).await.unwrap() });
    let pool = Pool::new(
        identity(&ca, "client"),
        PeerTransport::Auto,
        Duration::from_secs(2),
    )
    .unwrap();
    let uri = format!("https://{address}");
    assert!(matches!(
        pool.call(&uri, pstore_types::TenantId(9), json!("reject"))
            .await,
        Err(Error::Protocol)
    ));
    assert_eq!(pool.stats().probes, 1);
    assert_eq!(pool.stats().h2, 1);
    tokio::time::sleep(Duration::from_millis(110)).await;
    assert_eq!(
        pool.call(&uri, pstore_types::TenantId(9), json!("replacement"))
            .await
            .unwrap()
            .body,
        json!("replacement")
    );
    assert_eq!(
        pool.stats().probes,
        1,
        "a rejected H2 reply must not erase UDP backoff"
    );
    assert_eq!(pool.stats().fallbacks, 1);
    assert_eq!(pool.stats().h2, 2);
    server.abort();
}

#[tokio::test]
async fn closed_admission_expired_probes_and_pinned_entries_never_start_connections() {
    use crate::peer_http::{Error, Protocol};
    let ca = Authority::new();
    let new_pool = || {
        Pool::with_capacity(
            identity(&ca, "client"),
            PeerTransport::Auto,
            Duration::from_secs(1),
            1,
        )
        .unwrap()
    };
    let pool = new_pool();
    pool.requests.close();
    assert!(matches!(
        pool.call(
            "https://127.0.0.1:1",
            pstore_types::TenantId(1),
            json!(null)
        )
        .await,
        Err(Error::Unavailable)
    ));
    assert_eq!(pool.stats().entries, 0);
    let pool = new_pool();
    pool.connections.close();
    assert!(matches!(
        pool.connect(
            "127.0.0.1:1".parse().unwrap(),
            "localhost",
            Protocol::H2,
            None
        )
        .await,
        Err(Error::Unavailable)
    ));
    let pool = new_pool();
    assert!(matches!(
        pool.connect(
            "127.0.0.1:1".parse().unwrap(),
            "localhost",
            Protocol::H3,
            Some(tokio::time::Instant::now())
        )
        .await,
        Err(Error::Deadline)
    ));
    assert_eq!(pool.stats().connections, 0);
    assert_eq!(pool.stats().probes, 0);
    let pinned = pool.slot("https://one:443").unwrap();
    assert!(matches!(
        pool.slot("https://two:443"),
        Err(Error::Unavailable)
    ));
    assert!(matches!(
        pool.call(
            "https://127.0.0.1:1",
            pstore_types::TenantId(1),
            json!(null)
        )
        .await,
        Err(Error::Unavailable)
    ));
    assert_eq!(pool.stats().entries, 1);
    drop(pinned);
    assert!(pool.slot("https://two:443").is_ok());
    assert_eq!(pool.stats().entries, 1);
    let debug = format!("{pool:?}");
    assert!(debug.contains("PeerPool"));
    assert!(!debug.contains("PRIVATE KEY"));
}

#[tokio::test]
async fn exhausted_request_ids_refuse_dispatch_and_do_not_wrap_to_zero() {
    use std::sync::atomic::Ordering;
    for mode in [PeerTransport::H2, PeerTransport::H3] {
        let ca = Authority::new();
        let (uri, server, _) = start(&ca, false).await;
        let pool = Pool::new(identity(&ca, "client"), mode, Duration::from_secs(2)).unwrap();
        assert_eq!(
            pool.call(&uri, pstore_types::TenantId(1), json!("first"))
                .await
                .unwrap()
                .id,
            1
        );
        pool.ids.store(u64::MAX, Ordering::Relaxed);
        assert!(matches!(
            pool.call(&uri, pstore_types::TenantId(1), json!("must-not-dispatch"))
                .await,
            Err(crate::peer_http::Error::Protocol)
        ));
        assert_eq!(pool.ids.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(pool.stats().h2 + pool.stats().h3, 1);
        server.abort();
    }
}

#[tokio::test]
async fn a_timed_out_rpc_releases_admission_and_does_not_poison_later_calls() {
    for mode in [PeerTransport::H2, PeerTransport::H3] {
        let ca = Authority::new();
        let (tcp, udp) = crate::peer_http::tests::same_port().await;
        let address = tcp.local_addr().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Running(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Running {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let handler: Handler = {
            let (entered, active) = (entered.clone(), active.clone());
            Arc::new(move |_, req| {
                let (entered, active) = (entered.clone(), active.clone());
                Box::pin(async move {
                    active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _running = Running(active);
                    if req.body == json!("stall") {
                        entered.notify_one();
                        std::future::pending::<()>().await;
                    }
                    Reply {
                        version: VERSION,
                        id: req.id,
                        status: 200,
                        body: req.body,
                    }
                })
            })
        };
        let tls = identity(&ca, "127.0.0.1");
        let server = tokio::spawn(async move { serve(tcp, udp, tls, handler).await.unwrap() });
        let uri = format!("https://{address}");
        let pool = Pool::new(identity(&ca, "client"), mode, Duration::from_millis(500)).unwrap();
        let stalled = {
            let (pool, uri) = (pool.clone(), uri.clone());
            tokio::spawn(async move {
                pool.call(&uri, pstore_types::TenantId(9), json!("stall"))
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert!(stalled.await.unwrap().is_err());
        let reply = pool
            .call(&uri, pstore_types::TenantId(9), json!("healthy"))
            .await
            .unwrap();
        assert_eq!(reply.body, json!("healthy"));
        assert_eq!(pool.requests.available_permits(), 256);
        assert_eq!(
            pool.budget.available_permits(),
            crate::peer_wire::WIRE_BUDGET
        );
        tokio::time::timeout(Duration::from_secs(4), async {
            while active.load(std::sync::atomic::Ordering::SeqCst) != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled peer handler must eventually release its resources");
        server.abort();
    }
}

fn identity(ca: &Authority, name: &str) -> PeerTls {
    let m = ca.issue(&[name], false);
    PeerTls::from_pem(m.ca.as_bytes(), m.cert.as_bytes(), m.key.as_bytes()).unwrap()
}

async fn start(
    ca: &Authority,
    blackhole: bool,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    Option<std::net::UdpSocket>,
) {
    let (tcp, advertised_udp) = crate::peer_http::tests::same_port().await;
    let address = tcp.local_addr().unwrap();
    let (udp, held) = if blackhole {
        (
            std::net::UdpSocket::bind("127.0.0.1:0").unwrap(),
            Some(advertised_udp),
        )
    } else {
        (advertised_udp, None)
    };
    let handler: Handler = Arc::new(|protocol, req| {
        Box::pin(async move {
            Reply {
                version: VERSION,
                id: req.id,
                status: 200,
                body: json!({"protocol":format!("{protocol:?}"),"value":req.body}),
            }
        })
    });
    let tls = identity(ca, "127.0.0.1");
    let task = tokio::spawn(async move {
        serve(tcp, udp, tls, handler).await.unwrap();
    });
    (format!("https://{address}"), task, held)
}

#[tokio::test]
async fn auto_fallback_uses_the_current_call_deadline_after_configuration_changes() {
    let ca = Authority::new();
    let (uri, server, _blackhole) = start(&ca, true).await;
    let pool = Pool::new(
        identity(&ca, "client"),
        PeerTransport::Auto,
        Duration::from_secs(2),
    )
    .unwrap();
    let reply: crate::peer_wire::Reply = pool
        .call_with_timeout(
            &uri,
            pstore_types::TenantId(9),
            json!("shorter current cluster deadline"),
            Duration::from_millis(200),
        )
        .await
        .expect("a reused pool must leave time for H2 within the current call deadline");
    assert_eq!(reply.body["protocol"], "H2");
    assert_eq!((pool.stats().probes, pool.stats().fallbacks), (1, 1));
    server.abort();
}

#[tokio::test]
async fn auto_prefers_http3_and_reuses_the_authenticated_connection() {
    let ca = Authority::new();
    let (uri, server, _) = start(&ca, false).await;
    let pool = Pool::new(
        identity(&ca, "client"),
        PeerTransport::Auto,
        Duration::from_secs(2),
    )
    .unwrap();
    for i in 0..3 {
        let result = pool
            .call(&uri, pstore_types::TenantId(9), json!({"i":i}))
            .await
            .unwrap();
        assert_eq!(result.body["protocol"], "H3");
        assert_eq!(result.body["value"], json!({"i":i}));
    }
    let stats = pool.stats();
    assert_eq!(
        (stats.h3, stats.h2, stats.fallbacks, stats.probes),
        (1, 0, 0, 1)
    );
    assert_eq!((stats.entries, stats.connections), (1, 1));
    server.abort();
}

#[tokio::test]
async fn unavailable_udp_falls_back_with_one_deadline_and_caches_http2() {
    let ca = Authority::new();
    let (uri, server, _blackhole) = start(&ca, true).await;
    let pool = Pool::new(
        identity(&ca, "client"),
        PeerTransport::Auto,
        Duration::from_secs(2),
    )
    .unwrap();
    for _ in 0..2 {
        let reply = tokio::time::timeout(
            Duration::from_secs(2),
            pool.call(&uri, pstore_types::TenantId(9), json!({})),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(reply.body["protocol"], "H2");
    }
    let stats = pool.stats();
    assert_eq!(
        (stats.h3, stats.h2, stats.fallbacks, stats.probes),
        (0, 1, 1, 1)
    );
    server.abort();
}

#[tokio::test]
async fn invalid_server_trust_never_retries_http2() {
    let trusted = Authority::new();
    let (uri, server, _) = start(&Authority::new(), false).await;
    let pool = Pool::new(
        identity(&trusted, "client"),
        PeerTransport::Auto,
        Duration::from_secs(2),
    )
    .unwrap();
    assert!(
        pool.call(&uri, pstore_types::TenantId(9), json!({}))
            .await
            .is_err()
    );
    let stats = pool.stats();
    assert_eq!((stats.fallbacks, stats.h2), (0, 0));
    server.abort();
}

#[tokio::test]
async fn departed_members_and_idle_connections_release_all_pool_resources() {
    let ca = Authority::new();
    let (uri, server, _) = start(&ca, false).await;
    let pool = Pool::new(
        identity(&ca, "client"),
        PeerTransport::H2,
        Duration::from_secs(2),
    )
    .unwrap();
    pool.call(&uri, pstore_types::TenantId(9), json!({}))
        .await
        .unwrap();
    pool.retain(&[]);
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    assert_eq!((pool.stats().entries, pool.stats().connections), (0, 0));
    pool.call(&uri, pstore_types::TenantId(9), json!({}))
        .await
        .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(36)).await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    assert_eq!((pool.stats().entries, pool.stats().connections), (0, 0));
    tokio::time::resume();
    server.abort();
}

#[tokio::test]
async fn idle_lru_eviction_keeps_the_connection_and_entry_ceiling() {
    let ca = Authority::new();
    let pool = Pool::with_capacity(
        identity(&ca, "client"),
        PeerTransport::H2,
        Duration::from_secs(2),
        2,
    )
    .unwrap();
    let mut servers = Vec::new();
    for _ in 0..3 {
        let (uri, server, _) = start(&ca, false).await;
        pool.call(&uri, pstore_types::TenantId(9), json!({}))
            .await
            .unwrap();
        assert!(pool.stats().entries <= 2);
        assert!(pool.stats().connections <= 2);
        servers.push(server);
    }
    assert_eq!(pool.stats().h2, 3);
    for server in servers {
        server.abort();
    }
}

#[tokio::test]
async fn plaintext_credentials_paths_and_fragments_are_not_peer_addresses() {
    let ca = Authority::new();
    let pool = Pool::new(
        identity(&ca, "client"),
        PeerTransport::Auto,
        Duration::from_secs(1),
    )
    .unwrap();
    for uri in [
        "http://127.0.0.1:1",
        "https://user:secret@127.0.0.1:1",
        "https://127.0.0.1:1/path",
        "https://127.0.0.1:1?x=1",
        "https://127.0.0.1:1/#x",
    ] {
        assert!(
            pool.call(uri, pstore_types::TenantId(9), json!({}))
                .await
                .is_err()
        );
        assert_eq!(pool.stats().entries, 0);
        assert_eq!(pool.stats().probes, 0);
    }
}

#[tokio::test]
async fn waiting_for_local_capacity_is_not_an_http3_availability_probe() {
    let ca = Authority::new();
    let pool = Pool::with_capacity(
        identity(&ca, "client"),
        PeerTransport::Auto,
        Duration::from_secs(1),
        2,
    )
    .unwrap();
    let mut held = Vec::new();
    let mut servers = Vec::new();
    let mut uri = String::new();
    for _ in 0..2 {
        let (address, server, _) = start(&ca, false).await;
        pool.call(&address, pstore_types::TenantId(9), json!({}))
            .await
            .unwrap();
        let address = super::Address::parse(&address).unwrap();
        let slot = pool.slot(&address.key).unwrap();
        held.push(pool.client(&slot, &address).await.unwrap());
        servers.push(server);
        uri = address.key;
    }
    assert_eq!(pool.stats().probes, 2);
    pool.retain(&[]);
    // The removed members' callers still hold their live connections, so all
    // admission permits remain occupied until those callers release them.
    assert!(
        pool.call(&uri, pstore_types::TenantId(9), json!({}))
            .await
            .is_err()
    );
    assert_eq!(
        pool.stats().probes,
        2,
        "queueing must not send or count a probe"
    );
    assert_eq!(
        pool.stats().fallbacks,
        0,
        "local congestion is not UDP unavailability"
    );
    drop(held);
    for server in servers {
        server.abort();
    }
}

#[test]
fn peer_authorities_are_canonical_and_refuse_ambiguous_metadata() {
    use super::Address;
    for (input, expected) in [
        ("https://EXAMPLE.com/", "https://example.com:443"),
        ("https://[::1]", "https://[::1]:443"),
        ("https://[::1]:9443/", "https://[::1]:9443"),
        ("https://example.com:9443", "https://example.com:9443"),
    ] {
        assert_eq!(Address::parse(input).unwrap().key, expected);
    }
    let long = format!("https://{}", "a".repeat(1024));
    for input in [
        "http://example.com",
        "https://user:PRIVATE-KEY@example.com",
        "https://example.com/path",
        "https://example.com/?q=1",
        "https://example.com/#secret",
        "https://example.com:0",
        "https://example.com:65536",
        "https://example.com:invalid",
        "https://:443",
        &long,
    ] {
        let error = Address::parse(input).err().expect("invalid peer authority");
        assert!(!format!("{error:?}").contains("PRIVATE-KEY"));
    }
}

#[test]
fn impossible_deadlines_and_missing_runtime_do_not_start_peer_work() {
    use futures_util::FutureExt;
    let ca = Authority::new();
    let tls = identity(&ca, "client");
    for capacity in [0, 65] {
        assert!(
            Pool::with_capacity(
                tls.clone(),
                PeerTransport::Auto,
                Duration::from_secs(1),
                capacity
            )
            .is_err()
        );
    }
    assert!(Pool::new(tls.clone(), PeerTransport::Auto, Duration::ZERO).is_err());
    let pool = Pool::new(tls, PeerTransport::Auto, Duration::from_secs(1)).unwrap();
    for timeout in [Duration::ZERO, Duration::MAX] {
        let result = pool
            .call_with_timeout::<_, serde_json::Value>(
                "https://127.0.0.1:443",
                pstore_types::TenantId(9),
                json!(null),
                timeout,
            )
            .now_or_never()
            .unwrap();
        assert!(matches!(result, Err(super::Error::Deadline)));
    }
    let result = pool
        .call(
            "https://127.0.0.1:443",
            pstore_types::TenantId(9),
            json!(null),
        )
        .now_or_never()
        .unwrap();
    assert!(matches!(result, Err(super::Error::Unavailable)));
    let stats = pool.stats();
    assert_eq!(stats.entries, 0);
    assert_eq!(stats.connections, 0);
    assert_eq!(stats.probes, 0);
    assert_eq!(stats.fallbacks, 0);
}

#[test]
fn a_poisoned_pool_retains_identity_and_reclaims_departed_entries() {
    let ca = Authority::new();
    let pool = Pool::new(
        identity(&ca, "client"),
        PeerTransport::Auto,
        Duration::from_secs(1),
    )
    .unwrap();
    let uri = "https://example.com:443";
    let first = pool.slot(uri).unwrap();
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _state = pool.entries.lock().unwrap();
        panic!("inject pool-map poisoning");
    }));
    assert!(poisoned.is_err());
    assert_eq!(pool.stats().entries, 1);
    let second = pool.slot(uri).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    pool.retain(&[uri.to_owned()]);
    assert_eq!(pool.stats().entries, 1);
    pool.reap(tokio::time::Instant::now() + Duration::from_secs(60));
    assert_eq!(
        pool.stats().entries,
        1,
        "an in-flight slot survives idle sweeping"
    );
    drop(first);
    drop(second);
    pool.reap(tokio::time::Instant::now() + Duration::from_secs(60));
    assert_eq!(pool.stats().entries, 0);
    let _third = pool.slot(uri).unwrap();
    pool.retain(&[]);
    assert_eq!(pool.stats().entries, 0);
}

#[tokio::test]
async fn a_late_error_from_an_old_connection_cannot_evict_its_replacement() {
    for mode in [PeerTransport::H2, PeerTransport::H3] {
        let ca = Authority::new();
        let (tcp, udp) = crate::peer_http::tests::same_port().await;
        let address = tcp.local_addr().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let handler: Handler = {
            let (entered, release) = (entered.clone(), release.clone());
            Arc::new(move |_, request| {
                let (entered, release) = (entered.clone(), release.clone());
                Box::pin(async move {
                    if request.body == json!("old") {
                        entered.notify_one();
                        release.notified().await;
                    }
                    let invalid = request.body == json!("old") || request.body == json!("evict");
                    Reply {
                        version: VERSION,
                        id: request.id + u64::from(invalid),
                        status: 200,
                        body: request.body,
                    }
                })
            })
        };
        let server_identity = identity(&ca, "127.0.0.1");
        let server =
            tokio::spawn(async move { serve(tcp, udp, server_identity, handler).await.unwrap() });
        let pool = Pool::new(identity(&ca, "client"), mode, Duration::from_secs(5)).unwrap();
        let uri = format!("https://{address}");
        let old = {
            let (pool, uri) = (pool.clone(), uri.clone());
            tokio::spawn(async move {
                pool.call(&uri, pstore_types::TenantId(9), json!("old"))
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert!(matches!(
            pool.call(&uri, pstore_types::TenantId(9), json!("evict"))
                .await,
            Err(super::Error::Protocol)
        ));
        let slot = pool.slot(&uri).unwrap();
        let retry = slot.state.lock().await.retry.unwrap();
        tokio::time::timeout(Duration::from_secs(1), tokio::time::sleep_until(retry))
            .await
            .unwrap();
        assert_eq!(
            pool.call(&uri, pstore_types::TenantId(9), json!("fresh"))
                .await
                .unwrap()
                .body,
            json!("fresh")
        );
        release.notify_one();
        assert!(matches!(old.await.unwrap(), Err(super::Error::Protocol)));
        assert_eq!(
            pool.call(&uri, pstore_types::TenantId(9), json!("after"))
                .await
                .unwrap()
                .body,
            json!("after")
        );
        assert_eq!(
            pool.stats().h2 + pool.stats().h3,
            2,
            "the fresh connection must remain cached"
        );
        server.abort();
    }
}
