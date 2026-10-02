//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The engine registry's cap (M26). A tenant is made idle the way production does: written,
//! flushed, folded, and reaped at age 0.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, Faults, Faulty, MemoryStore, OpClass};
use pstore_server::{Api, FoldPolicy, GcPolicy};
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;
/// A process over a store whose faults a test switches.
type F = Arc<Api<Faulty<MemoryStore>>>;

async fn send<S: pstore_blob::BlobStore + 'static>(
    api: &Arc<Api<S>>,
    tenant: u64,
    method: &str,
    uri: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant.to_string())
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn write<S: pstore_blob::BlobStore + 'static>(
    api: &Arc<Api<S>>,
    tenant: u64,
    id: &str,
    durability: &str,
) {
    let body = json!({"durability": durability, "documents": [{"id": id, "vector": [1.0, 0.5]}]});
    let (s, b) = send(api, tenant, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn ids(api: &A, tenant: u64) -> Vec<String> {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 100});
    let (s, b) = send(api, tenant, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn now_due() -> (FoldPolicy, GcPolicy) {
    (
        FoldPolicy {
            period: Duration::from_secs(1),
            age: Duration::ZERO,
            bytes: 0,
        },
        GcPolicy {
            period: Duration::from_secs(1),
            age: Duration::ZERO,
        },
    )
}

/// `tenants` written, folded and reaped: idle, as production leaves them.
async fn settled(api: &A, tenants: std::ops::Range<u64>) {
    let (fold, gc) = now_due();
    for t in tenants {
        write(api, t, &format!("d{t}"), "durable").await;
    }
    api.fold_due(&fold).await;
    api.reap_due(&gc).await;
}

fn api(cap: usize) -> (Accounted<MemoryStore>, A) {
    let store = Accounted::new(MemoryStore::new());
    let a = Api::new(store.clone(), LaneId(1)).unwrap();
    a.limit_engines(cap);
    a.set_reaping(true);
    (store, a)
}

#[tokio::test(start_paused = true)]
async fn the_registry_is_bounded() {
    let (_, a) = api(4);
    settled(&a, 100..120).await;
    assert_eq!(a.engines().await, 20);
    let tick = a.evict_tick().await;
    assert_eq!(tick.evicted, 16, "{tick:?}");
    assert_eq!(a.engines().await, 4, "cap - cap / 10 is 4");
    assert_eq!(a.eviction_counts().0, 16);
    // And with a margin: 30 tenants under a cap of 20 leave 18.
    let (_, b) = api(20);
    settled(&b, 100..130).await;
    b.evict_tick().await;
    assert_eq!(b.engines().await, 18, "cap - cap / 10 is 18");
}

#[tokio::test(start_paused = true)]
async fn unflushed_rows_are_never_evicted() {
    // 200 is written first: the least recently used, so only `pending` can keep it.
    let (_, a) = api(1);
    write(&a, 200, "pending", "batched").await;
    tokio::time::advance(Duration::from_secs(1)).await;
    settled(&a, 100..103).await;
    for _ in 0..3 {
        tokio::time::advance(Duration::from_secs(61)).await;
        a.evict_tick().await;
    }
    assert_eq!(a.engines().await, 1, "only the busy one is left");
    assert_eq!(ids(&a, 200).await, ["pending"]);
}

/// Two processes over one store with switchable faults; `a`'s cap is 1.
fn two() -> (Faulty<MemoryStore>, F, F) {
    let faulty = Faulty::new(MemoryStore::new(), 1, Faults::none());
    let store = Accounted::new(faulty.clone());
    let a = Api::new(store.clone(), LaneId(1)).unwrap();
    let other = Api::new(store, LaneId(2)).unwrap();
    a.limit_engines(1);
    (faulty, a, other)
}

/// `a` asked to fold tenant 300 by a refused `strong` read: its unfolded rows are `other`'s,
/// so `a`'s own engine for it holds nothing.
async fn requested(a: &F, other: &F) {
    for t in 100..102 {
        write(a, t, "x", "durable").await;
    }
    let (fold, _) = now_due();
    a.fold_due(&fold).await;
    write(other, 300, "theirs", "durable").await;
    let q = json!({"rank_by": ["id", "asc"], "top_k": 10, "consistency": "strong"});
    let (s, b) = send(a, 300, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert!(a.hold_engine_for_test(TenantId(300)).await.is_idle(false));
}

/// Uses 100 and 101 after 300, so 300 is the least recently used: the first to go if nothing
/// kept it.
async fn fresher(a: &F) {
    tokio::time::advance(Duration::from_secs(1)).await;
    for t in [100, 101] {
        let _ = a.hold_engine_for_test(TenantId(t)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_requested_or_backing_off_tenant_is_kept() {
    // Requested: kept, though its engine is idle.
    let (_, a, other) = two();
    requested(&a, &other).await;
    fresher(&a).await;
    a.evict_tick().await;
    assert_eq!(a.eviction_counts().0, 2);
    let _ = a.hold_engine_for_test(TenantId(300)).await;
    assert_eq!(a.engines().await, 1, "the requested tenant was evicted");

    // Backing off: its fold was taken, failed, and is due again later -- kept until then.
    let (faulty, a, other) = two();
    requested(&a, &other).await;
    faulty.set_faults(Faults {
        cas_contended: 1.0,
        ..Faults::none()
    });
    let (fold, _) = now_due();
    a.fold_due(&fold).await;
    faulty.set_faults(Faults::none());
    assert!(a.hold_engine_for_test(TenantId(300)).await.is_idle(false));
    fresher(&a).await;
    a.evict_tick().await;
    assert_eq!(a.eviction_counts().0, 2);
    let _ = a.hold_engine_for_test(TenantId(300)).await;
    assert_eq!(a.engines().await, 1, "the backing-off tenant was evicted");
}

#[tokio::test(start_paused = true)]
async fn an_engine_in_use_is_not_evicted() {
    // 100 is the least recently used, and held: the other two go instead.
    let (_, a) = api(1);
    settled(&a, 100..103).await;
    let held = a.hold_engine_for_test(TenantId(100)).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    for t in [101, 102] {
        let _ = a.hold_engine_for_test(TenantId(t)).await;
    }
    a.evict_tick().await;
    assert_eq!(a.engines().await, 1);
    let again = a.hold_engine_for_test(TenantId(100)).await;
    assert!(
        Arc::ptr_eq(&held, &again),
        "a held engine was evicted and rebuilt"
    );
}

#[tokio::test(start_paused = true)]
async fn the_least_recently_used_goes_first() {
    // Built in order 100, 101, 102; then 100 is used again, so 101 is the least recent.
    let (_, a) = api(2);
    settled(&a, 100..103).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    let _ = a.hold_engine_for_test(TenantId(100)).await;
    let tick = a.evict_tick().await;
    assert_eq!(tick.evicted, 1);
    // 101 went: asking for it again builds an engine; asking for 100 or 102 does not.
    for t in [100, 102] {
        let _ = a.hold_engine_for_test(TenantId(t)).await;
        assert_eq!(a.engines().await, 2, "{t} was evicted");
    }
    let _ = a.hold_engine_for_test(TenantId(101)).await;
    assert_eq!(a.engines().await, 3, "101 was not the one evicted");
}

#[tokio::test(start_paused = true)]
async fn an_evicted_tenant_comes_back_whole() {
    let (_, a) = api(1);
    settled(&a, 100..103).await;
    a.evict_tick().await;
    assert_eq!(a.eviction_counts().0, 2, "nothing was evicted");
    // Whichever was evicted, every tenant still has its row, and writes, flushes and folds.
    for t in 100..103 {
        assert_eq!(ids(&a, t).await, [format!("d{t}")]);
        write(&a, t, &format!("e{t}"), "durable").await;
    }
    let (fold, _) = now_due();
    a.fold_due(&fold).await;
    for t in 100..103 {
        assert_eq!(ids(&a, t).await, [format!("d{t}"), format!("e{t}")]);
    }
}

#[tokio::test(start_paused = true)]
async fn the_cap_is_soft_and_a_fruitless_scan_backs_off() {
    let (_, a) = api(1);
    for t in 100..103 {
        write(&a, t, &format!("p{t}"), "batched").await;
    }
    // Over the cap with nothing idle: every request is still served.
    assert_eq!(a.engines().await, 3);
    let first = a.evict_tick().await;
    assert!(first.scanned && first.evicted == 0, "{first:?}");
    // Within the backoff: no scan.
    let next = a.evict_tick().await;
    assert!(!next.scanned, "a fruitless scan was repeated at once");
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(a.evict_tick().await.scanned, "the backoff never ended");
    assert_eq!(a.eviction_counts().1, 2);
}

#[tokio::test(start_paused = true)]
async fn eviction_runs_with_fold_and_gc_off() {
    // No reap loop: a reapable commit keeps nothing, so a tenant that committed is evictable.
    let store = Accounted::new(MemoryStore::new());
    let a = Api::new(store, LaneId(1)).unwrap();
    a.limit_engines(1);
    let (fold, _) = now_due();
    for t in 100..103 {
        write(&a, t, "x", "durable").await;
    }
    a.fold_due(&fold).await;
    let stop = async {
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    pstore_server::run_evictions(Arc::clone(&a), stop).await;
    assert_eq!(a.engines().await, 1);
}

#[tokio::test(start_paused = true)]
async fn eviction_costs_no_request() {
    let (store, a) = api(1);
    settled(&a, 100..105).await;
    let t = TenantId(100);
    let before = (
        store.count(t, OpClass::Read),
        store.count(t, OpClass::Write),
    );
    let all: u64 = (100..105)
        .map(|n| store.count(TenantId(n), OpClass::Read) + store.count(TenantId(n), OpClass::Write))
        .sum();
    a.evict_tick().await;
    let after: u64 = (100..105)
        .map(|n| store.count(TenantId(n), OpClass::Read) + store.count(TenantId(n), OpClass::Write))
        .sum();
    assert_eq!(after, all);
    assert_eq!(
        (
            store.count(t, OpClass::Read),
            store.count(t, OpClass::Write)
        ),
        before
    );
}

#[tokio::test(start_paused = true)]
async fn an_idle_engine_held_does_not_back_off() {
    // Every idle engine is held a moment -- as a tick's snapshot holds them -- so the pass
    // frees nothing, and must not count that against the next.
    let (_, a) = api(1);
    settled(&a, 100..103).await;
    let held: Vec<_> =
        futures_util::future::join_all((100..103).map(|t| a.hold_engine_for_test(TenantId(t))))
            .await;
    let first = a.evict_tick().await;
    assert!(first.scanned && first.evicted == 0, "{first:?}");
    drop(held);
    let next = a.evict_tick().await;
    assert!(
        next.scanned,
        "a pass that found held idle engines backed off"
    );
    assert_eq!(next.evicted, 2);
}

#[tokio::test(start_paused = true)]
async fn a_fruitless_scan_doubles_its_backoff() {
    let (_, a) = api(1);
    for t in 100..103 {
        write(&a, t, "p", "batched").await;
    }
    assert!(a.evict_tick().await.scanned);
    // 1 s, exactly: due again.
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(a.evict_tick().await.scanned, "not due at its backoff");
    // Now 2 s: one second is too soon, two is due.
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(!a.evict_tick().await.scanned, "the backoff did not double");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(a.evict_tick().await.scanned);
    assert_eq!(a.eviction_counts().1, 3);
}

#[tokio::test(start_paused = true)]
async fn a_registry_at_its_cap_is_not_scanned() {
    let (_, a) = api(4);
    settled(&a, 100..104).await;
    assert_eq!(a.engines().await, 4);
    assert!(
        !a.evict_tick().await.scanned,
        "a registry at its cap was scanned"
    );
    assert_eq!(a.eviction_counts().1, 0);
}

/// `serve_folding` with a cap of 1 over three committed tenants, for three seconds, with `gc`.
async fn served(gc: Option<GcPolicy>) -> (A, String) {
    let store = Accounted::new(MemoryStore::new());
    let a = Api::new(store, LaneId(1)).unwrap();
    a.limit_engines(1);
    for t in 100..103 {
        write(&a, t, "x", "durable").await;
        let (s, b) = send(&a, t, "POST", "/v1/admin/fold", &json!({})).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stop = tokio::time::sleep(Duration::from_secs(3));
    pstore_server::serve_folding(Arc::clone(&a), listener, stop, None, gc, false)
        .await
        .unwrap();
    let (s, _) = send(&a, 100, "GET", "/metrics", &Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    let req = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let res = Arc::clone(&a).router().oneshot(req).await.unwrap();
    let text =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    (a, text)
}

#[tokio::test(start_paused = true)]
async fn a_served_registry_keeps_what_its_reap_loop_owes() {
    // GC on, with an hour's age: every tenant has a commit the reap loop has yet to collect.
    let gc = GcPolicy {
        period: Duration::from_secs(1),
        age: Duration::from_secs(3600),
    };
    let (a, metrics) = served(Some(gc)).await;
    assert_eq!(
        a.engines().await,
        3,
        "a tenant the reap loop owes was evicted"
    );
    assert!(metrics.contains("pstore_engines 3"), "{metrics}");
    // Fold and GC off: nothing would collect those commits, so they keep nothing.
    let (a, metrics) = served(None).await;
    assert_eq!(a.engines().await, 1);
    assert!(metrics.contains("pstore_engines 1"), "{metrics}");
    assert!(
        metrics.contains("pstore_engines_evicted_total 2"),
        "{metrics}"
    );
}
