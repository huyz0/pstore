//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A fold scheduled inside the server — M9i.1. Another process sees a durable write without an
//! operator's fold, a tenant with nothing to fold costs no request, and a failing fold backs
//! off rather than retrying every tick.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, OpClass,
    Precondition, PutOutcome,
};
use pstore_server::{Api, Config, FoldPolicy, run_folds, serve_folding};
use pstore_types::{CasTag, LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tower::ServiceExt;

/// A store that can fail, or hold, the requests whose key contains a pattern. Cloned, it is
/// the same store: every server of a world shares one.
#[derive(Debug, Default, Clone)]
struct Switch(Arc<Inner>);

impl std::ops::Deref for Switch {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

#[derive(Debug, Default)]
struct Inner {
    inner: MemoryStore,
    /// Reads of a key containing this fail.
    fail: Mutex<Option<String>>,
    /// A read (`get_with_tag`) or a `put` of a key containing this waits for `release`.
    hold: Mutex<Option<String>>,
    held: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl Inner {
    fn failing(&self, key: &Key) -> bool {
        self.fail
            .lock()
            .unwrap()
            .as_deref()
            .is_some_and(|p| key.as_str().contains(p))
    }
    async fn maybe_hold(&self, key: &Key) {
        let hit = self
            .hold
            .lock()
            .unwrap()
            .as_deref()
            .is_some_and(|p| key.as_str().contains(p));
        if hit {
            *self.hold.lock().unwrap() = None;
            self.held.notify_one();
            self.release.notified().await;
        }
    }
    fn gate(&self, key: &Key) -> Result<(), BlobError> {
        if self.failing(key) {
            Err(BlobError::Other("injected read failure".to_owned()))
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for Switch {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.gate(key)?;
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.gate(key)?;
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.gate(key)?;
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.gate(key)?;
        self.maybe_hold(key).await;
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.gate(key)?;
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        // Held AFTER the write lands: the bundle is in the store, and the flush has not yet
        // heard so -- the window a concurrent fold can fold it in.
        let out = self.inner.put(key, body).await;
        self.maybe_hold(key).await;
        out
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        // A bundle is created conditionally since M17, so its write is held here too.
        let out = self.inner.put_conditional(key, body, pre).await;
        if key.as_str().ends_with(".bundle") {
            self.maybe_hold(key).await;
        }
        out
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

/// One shared backend, and a server on it per lane.
struct World {
    switch: Switch,
    stores: Vec<Accounted<Switch>>,
    apis: Vec<Arc<Api<Switch>>>,
}

fn world(servers: u64) -> World {
    let switch = Switch::default();
    let mut stores = Vec::new();
    let mut apis = Vec::new();
    for lane in 1..=servers {
        let store = Accounted::new(switch.clone());
        apis.push(Api::new(store.clone(), LaneId(lane)).unwrap());
        stores.push(store);
    }
    World {
        switch,
        stores,
        apis,
    }
}

impl World {
    /// Every request any server billed to `tenant`.
    fn requests(&self, tenant: u64) -> u64 {
        self.stores
            .iter()
            .map(|s| {
                [OpClass::Read, OpClass::Write, OpClass::List]
                    .into_iter()
                    .map(|c| s.count(TenantId(u128::from(tenant)), c))
                    .sum::<u64>()
            })
            .sum()
    }
}

async fn send<S: BlobStore>(api: &Arc<Api<S>>, req: Request<Body>) -> (StatusCode, Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()));
    (status, v)
}

fn request(tenant: u64, method: &str, uri: &str, body: Option<&Value>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant.to_string())
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn write<S: BlobStore>(api: &Arc<Api<S>>, tenant: u64, ids: &[&str], durable: bool) {
    let docs: Vec<Value> = ids
        .iter()
        .map(|id| json!({"id": id, "vector": [1.0, 0.5], "text": "x".repeat(200)}))
        .collect();
    let d = if durable { "durable" } else { "batched" };
    let body = json!({"durability": d, "documents": docs});
    let (s, b) = send(
        api,
        request(tenant, "PUT", "/v1/indexes/docs/documents", Some(&body)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// The ids another process finds, or none when the index does not exist for it yet.
async fn found<S: BlobStore>(api: &Arc<Api<S>>, tenant: u64) -> Vec<String> {
    let q = json!({"vector": [1.0, 0.5], "top_k": 100});
    let (s, b) = send(
        api,
        request(tenant, "POST", "/v1/indexes/docs/query", Some(&q)),
    )
    .await;
    if s == StatusCode::NOT_FOUND {
        return Vec::new();
    }
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut ids: Vec<String> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

fn policy(age: Duration, bytes: u64) -> FoldPolicy {
    FoldPolicy {
        period: Duration::from_secs(1),
        age,
        bytes,
    }
}

const NOW: Duration = Duration::ZERO;
const HOUR: Duration = Duration::from_secs(3600);
const MIB: u64 = 1 << 20;

#[tokio::test]
async fn another_process_sees_a_durable_write_with_no_admin_call() {
    let w = world(2);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    write(a, 11, &["x"], true).await;
    assert!(found(b, 11).await.is_empty(), "visible before any fold");
    // A query through the writer prunes at the watermark it reads; the live batch's stamp
    // must survive it (mutation sweep: `>=` flipped in `prune` dropped it, and nothing was due).
    assert_eq!(found(a, 11).await, ["x"]);
    let tick = a.fold_due(&policy(NOW, MIB)).await;
    assert_eq!(tick.folded, 1, "{tick:?}");
    assert_eq!(found(b, 11).await, ["x"]);
    let again = a.fold_due(&policy(NOW, MIB)).await;
    assert_eq!((again.folded, again.nothing), (0, 0), "{again:?}");
}

#[tokio::test]
async fn a_tenant_with_nothing_to_fold_costs_no_request() {
    let w = world(2);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    let due = policy(NOW, MIB);
    // (a) Folded by this process.
    write(a, 21, &["x"], true).await;
    a.fold_due(&due).await;
    let before = w.requests(21);
    a.fold_due(&due).await;
    assert_eq!(w.requests(21), before, "(a) a folded tenant was probed");
    // (b) Only batched rows: nothing durable, nothing to fold.
    write(a, 22, &["x"], false).await;
    let before = w.requests(22);
    let tick = a.fold_due(&due).await;
    assert_eq!(
        w.requests(22),
        before,
        "(b) a batched tenant was probed: {tick:?}"
    );
    // (c) Folded by ANOTHER process: at most one `nothing` fold, then silence.
    write(a, 23, &["x"], true).await;
    write(b, 23, &["y"], true).await;
    b.fold_due(&due).await;
    assert_eq!(found(b, 23).await, ["x", "y"]);
    let first = a.fold_due(&due).await;
    // B folded lane A's bundle too: A's fold finds nothing, and prunes (mutation sweep: the
    // `nothing` count was never asserted).
    assert_eq!((first.folded, first.nothing), (0, 1), "{first:?}");
    let before = w.requests(23);
    let second = a.fold_due(&due).await;
    assert_eq!(
        w.requests(23),
        before,
        "(c) a tenant another process folded stayed due: {second:?}"
    );
}

#[tokio::test]
async fn a_flush_that_lost_to_a_fold_leaves_nothing_due() {
    // (d) The bundle lands, a fold folds it, and only then does the flush record it.
    let w = world(1);
    let a = Arc::clone(&w.apis[0]);
    *w.switch.hold.lock().unwrap() = Some("/wal/24/".to_owned());
    let writer = {
        let a = Arc::clone(&a);
        tokio::spawn(async move { write(&a, 24, &["x"], true).await })
    };
    w.switch.held.notified().await;
    let (s, b) = send(&a, request(24, "POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    w.switch.release.notify_one();
    writer.await.unwrap();
    let before = w.requests(24);
    let tick = a.fold_due(&policy(NOW, MIB)).await;
    assert_eq!(
        w.requests(24),
        before,
        "a straggler kept the tenant due: {tick:?}"
    );
    assert_eq!(found(&a, 24).await, ["x"]);
}

#[tokio::test]
async fn the_byte_threshold_picks_the_larger_tenant() {
    let w = world(2);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    write(a, 31, &["s"], true).await;
    let many: Vec<String> = (0..40).map(|i| format!("d{i}")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    write(a, 32, &refs, true).await;
    // One document's bundle is a few hundred bytes; forty are several kilobytes.
    let tick = a.fold_due(&policy(HOUR, 4096)).await;
    assert_eq!(tick.folded, 1, "{tick:?}");
    assert_eq!(found(b, 32).await.len(), 40);
    assert!(found(b, 31).await.is_empty(), "the small tenant was folded");
}

#[tokio::test(start_paused = true)]
async fn the_age_threshold_is_exact() {
    let w = world(2);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    write(a, 41, &["x"], true).await;
    let minute = policy(Duration::from_secs(60), MIB);
    tokio::time::advance(Duration::from_secs(59)).await;
    assert_eq!(a.fold_due(&minute).await.folded, 0);
    assert!(found(b, 41).await.is_empty());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(a.fold_due(&minute).await.folded, 1);
    assert_eq!(found(b, 41).await, ["x"]);
}

#[tokio::test(start_paused = true)]
async fn age_is_measured_from_the_oldest_batch() {
    // Code review: with one batch, oldest and newest are the same batch.
    let w = world(2);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    write(a, 42, &["x"], true).await;
    tokio::time::advance(Duration::from_secs(30)).await;
    write(a, 42, &["y"], true).await;
    let minute = policy(Duration::from_secs(60), MIB);
    tokio::time::advance(Duration::from_secs(29)).await;
    assert_eq!(a.fold_due(&minute).await.folded, 0);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        a.fold_due(&minute).await.folded,
        1,
        "aged from the newest batch"
    );
    assert_eq!(found(b, 42).await, ["x", "y"]);
}

#[tokio::test]
async fn bytes_are_summed_across_batches_and_met_exactly() {
    let w = world(1);
    let a = &w.apis[0];
    // A first write and fold, so the lane is registered and later writes bill only bundles.
    write(a, 33, &["w"], true).await;
    a.fold_due(&policy(NOW, MIB)).await;
    let written = |w: &World| {
        w.stores
            .iter()
            .map(|s| s.bytes(TenantId(33), OpClass::Write))
            .sum::<u64>()
    };
    let before = written(&w);
    write(a, 33, &["x"], true).await;
    let one = written(&w) - before;
    write(a, 33, &["y"], true).await;
    let two = written(&w) - before;
    assert!(one > 0 && two > one);
    let at = |bytes| policy(HOUR, bytes);
    assert_eq!(
        a.fold_due(&at(two + 1)).await.folded,
        0,
        "one byte short folded"
    );
    assert_eq!(
        a.fold_due(&at(two)).await.folded,
        1,
        "the exact total did not fold"
    );
}

#[tokio::test(start_paused = true)]
async fn the_backoff_doubles_to_its_cap_and_resets() {
    let w = world(1);
    let a = &w.apis[0];
    write(a, 62, &["x"], true).await;
    *w.switch.fail.lock().unwrap() = Some("/tnt/62/HEAD".to_owned());
    let p = FoldPolicy {
        period: Duration::from_secs(1),
        age: Duration::from_secs(8),
        bytes: 1,
    };
    // Attempts at 0, 1, 3, 7, then every 8: 15, 23, 31, 39, 47, 55, 63.
    let mut attempts = Vec::new();
    for tick in 0..64 {
        if a.fold_due(&p).await.failed == 1 {
            attempts.push(tick);
        }
        tokio::time::advance(p.period).await;
    }
    assert_eq!(attempts, [0, 1, 3, 7, 15, 23, 31, 39, 47, 55, 63]);
    // A success resets it: the next failure is retried one period later, not eight.
    *w.switch.fail.lock().unwrap() = None;
    tokio::time::advance(p.age).await;
    assert_eq!(a.fold_due(&p).await.folded, 1);
    write(a, 62, &["y"], true).await;
    *w.switch.fail.lock().unwrap() = Some("/tnt/62/HEAD".to_owned());
    assert_eq!(a.fold_due(&p).await.failed, 1);
    tokio::time::advance(p.period).await;
    assert_eq!(a.fold_due(&p).await.failed, 1, "the backoff was not reset");
}

#[tokio::test(start_paused = true)]
async fn the_loop_folds_on_its_own_and_stops() {
    let w = world(2);
    let (a, b) = (Arc::clone(&w.apis[0]), Arc::clone(&w.apis[1]));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let fast = policy(Duration::from_secs(2), MIB);
    let looping = tokio::spawn(run_folds(Arc::clone(&a), fast, async move {
        let _ = stopped.await;
    }));
    write(&a, 51, &["x"], true).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        found(&b, 51).await,
        ["x"],
        "nothing folded within age + period"
    );
    stop.send(()).unwrap();
    looping.await.unwrap();
}

#[tokio::test]
async fn a_served_process_folds_and_stops_with_its_signal() {
    let w = world(2);
    let (a, b) = (Arc::clone(&w.apis[0]), Arc::clone(&w.apis[1]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let quick = FoldPolicy {
        period: Duration::from_millis(20),
        age: Duration::from_millis(50),
        bytes: MIB,
    };
    let server = tokio::spawn(serve_folding(
        Arc::clone(&a),
        listener,
        async move {
            let _ = stopped.await;
        },
        Some(quick),
        None,
    ));
    write(&a, 52, &["x"], true).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while found(&b, 52).await.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the served loop never folded"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    // Nothing folds after `serve_folding` returned, even with a write left due.
    write(&a, 53, &["y"], true).await;
    let before = w.requests(53);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        w.requests(53),
        before,
        "a fold ran after the server stopped"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_fold_backs_off_and_recovers() {
    let w = world(1);
    let a = &w.apis[0];
    write(a, 61, &["x"], true).await;
    *w.switch.fail.lock().unwrap() = Some("/tnt/61/HEAD".to_owned());
    let p = FoldPolicy {
        period: Duration::from_secs(1),
        age: Duration::from_secs(64),
        bytes: 1,
    };
    let (mut failed, mut deferred) = (0, 0);
    for tick in 0..64 {
        let t = a.fold_due(&p).await;
        if tick == 0 {
            assert_eq!(t.failed, 1, "{t:?}");
            let (_, m) = send(a, request(61, "GET", "/metrics", None)).await;
            let m = m.as_str().unwrap().to_owned();
            assert!(m.contains("pstore_fold_total{outcome=\"failed\"} 1"), "{m}");
        }
        failed += t.failed;
        deferred += t.deferred;
        tokio::time::advance(p.period).await;
    }
    assert!(failed <= 8, "{failed} attempts in 64 ticks");
    assert!(deferred > 0);
    *w.switch.fail.lock().unwrap() = None;
    let mut folded = 0;
    for _ in 0..70 {
        folded += a.fold_due(&p).await.folded;
        if folded > 0 {
            break;
        }
        tokio::time::advance(p.period).await;
    }
    assert_eq!(folded, 1, "never recovered");
}

#[tokio::test(start_paused = true)]
async fn an_absurd_period_backs_off_without_panicking() {
    // Past what an `Instant` can hold, the deadline must not overflow (mutation sweep).
    let w = world(1);
    let a = &w.apis[0];
    write(a, 63, &["x"], true).await;
    *w.switch.fail.lock().unwrap() = Some("/tnt/63/HEAD".to_owned());
    // `u64::MAX` seconds -- `PSTORE_FOLD_AGE_S` can say it -- is past what an `Instant` holds;
    // `u64::MAX` milliseconds is not, and never reached the fallback (mutation sweep).
    let p = FoldPolicy {
        period: Duration::from_secs(u64::MAX),
        age: Duration::from_secs(u64::MAX),
        bytes: 1,
    };
    assert_eq!(a.fold_due(&p).await.failed, 1);
    // Five years on, the fallback's decade has not passed: still deferred.
    tokio::time::advance(Duration::from_secs(5 * 365 * 86_400)).await;
    assert_eq!(
        a.fold_due(&p).await.deferred,
        1,
        "the fallback deadline came early"
    );
}

#[tokio::test]
async fn a_fold_in_flight_does_not_block_other_tenants() {
    let w = world(1);
    let a = Arc::clone(&w.apis[0]);
    write(&a, 71, &["x"], true).await;
    *w.switch.hold.lock().unwrap() = Some("/tnt/71/HEAD".to_owned());
    let folding = {
        let a = Arc::clone(&a);
        tokio::spawn(async move { a.fold_due(&policy(NOW, MIB)).await })
    };
    w.switch.held.notified().await;
    tokio::time::timeout(Duration::from_secs(5), write(&a, 72, &["y"], true))
        .await
        .expect("a write to another tenant waited behind a fold");
    w.switch.release.notify_one();
    assert_eq!(folding.await.unwrap().folded, 1);
}

#[tokio::test]
async fn two_processes_fold_one_tenant_without_failing() {
    let w = world(2);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    write(a, 81, &["x"], true).await;
    write(b, 81, &["y"], true).await;
    let due = policy(NOW, MIB);
    let (ta, tb) = tokio::join!(a.fold_due(&due), b.fold_due(&due));
    assert_eq!((ta.failed, tb.failed), (0, 0), "{ta:?} {tb:?}");
    assert_eq!(found(a, 81).await, ["x", "y"]);
    assert_eq!(found(b, 81).await, ["x", "y"]);
}

#[tokio::test]
async fn folds_are_counted_by_outcome() {
    let w = world(1);
    let a = &w.apis[0];
    write(a, 91, &["x"], true).await;
    a.fold_due(&policy(NOW, MIB)).await;
    let (_, m) = send(a, request(91, "GET", "/metrics", None)).await;
    let m = m.as_str().unwrap().to_owned();
    assert!(m.contains("pstore_fold_total{outcome=\"folded\"} 1"), "{m}");
    assert!(!m.contains("91"), "a tenant leaked into /metrics:\n{m}");
}

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |k| owned.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
}

#[test]
fn the_fold_policy_is_configured_or_refused() {
    let base = [("PSTORE_LANE", "1")];
    let c = Config::from_vars(vars(&base)).unwrap();
    assert_eq!(c.fold, Some(FoldPolicy::default()));
    assert_eq!(
        FoldPolicy::default(),
        FoldPolicy {
            period: Duration::from_secs(1),
            age: HOUR,
            bytes: MIB
        }
    );
    let off = Config::from_vars(vars(&[("PSTORE_LANE", "1"), ("PSTORE_FOLD", "off")])).unwrap();
    assert_eq!(off.fold, None);
    let set = Config::from_vars(vars(&[
        ("PSTORE_LANE", "1"),
        ("PSTORE_FOLD_PERIOD_MS", "250"),
        ("PSTORE_FOLD_AGE_S", "60"),
        ("PSTORE_FOLD_BYTES", "4096"),
    ]))
    .unwrap();
    assert_eq!(
        set.fold,
        Some(FoldPolicy {
            period: Duration::from_millis(250),
            age: Duration::from_secs(60),
            bytes: 4096
        })
    );
    // `off` does not excuse a bad value beside it.
    let e = Config::from_vars(vars(&[
        ("PSTORE_LANE", "1"),
        ("PSTORE_FOLD", "off"),
        ("PSTORE_FOLD_AGE_S", "0"),
    ]))
    .unwrap_err();
    assert!(e.to_string().contains("PSTORE_FOLD_AGE_S"), "{e}");
    for (var, value) in [
        ("PSTORE_FOLD_PERIOD_MS", "0"),
        ("PSTORE_FOLD_AGE_S", "-1"),
        ("PSTORE_FOLD_BYTES", "x"),
        ("PSTORE_FOLD", "on"),
    ] {
        let e = Config::from_vars(vars(&[("PSTORE_LANE", "1"), (var, value)])).unwrap_err();
        assert!(e.to_string().contains(var), "{var}={value}: {e}");
    }
}

#[tokio::test]
async fn the_fold_duty_tells_the_truth() {
    let w = world(1);
    let (s, b) = send(&w.apis[0], request(1, "GET", "/v1/admin/duties", None)).await;
    assert_eq!(s, StatusCode::OK);
    let text = b.to_string();
    assert!(!text.contains("read on every query"), "{text}");
    assert!(text.contains("scheduled"), "{text}");
    assert!(text.contains("died or restarted"), "{text}");
    let deploy =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/deploy.md"))
            .unwrap();
    assert!(!deploy.contains("read on every query"));
    assert!(deploy.contains("only writer died, or restarted"));
    for var in [
        "PSTORE_FOLD_PERIOD_MS",
        "PSTORE_FOLD_AGE_S",
        "PSTORE_FOLD_BYTES",
        "PSTORE_FOLD`",
    ] {
        assert!(deploy.contains(var), "deploy.md does not document {var}");
    }
}
