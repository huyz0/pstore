//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `consistency` — M9i.2. `eventual` is today's answer; `strong` answers only when it reflects
//! every durable write any process acknowledged, and otherwise refuses and asks for a fold.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, OpClass,
    Precondition, PutOutcome,
};
use pstore_server::{Api, FoldPolicy};
use pstore_testkit::depth::DepthCounting;
use pstore_types::{CasTag, LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
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
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

type Store = DepthCounting<Switch>;

/// One shared backend under a depth counter, and a server on it per lane.
struct World {
    switch: Switch,
    depth: Store,
    stores: Vec<Accounted<Store>>,
    apis: Vec<Arc<Api<Store>>>,
}

fn world(lanes: &[u64]) -> World {
    let switch = Switch::default();
    let depth = DepthCounting::new(switch.clone());
    let mut stores = Vec::new();
    let mut apis = Vec::new();
    for lane in lanes {
        let store = Accounted::new(depth.clone());
        apis.push(Api::new(store.clone(), LaneId(*lane)).unwrap());
        stores.push(store);
    }
    World {
        switch,
        depth,
        stores,
        apis,
    }
}

impl World {
    fn reads(&self, tenant: u64) -> (u64, u64, u64) {
        let t = TenantId(u128::from(tenant));
        let sum = |c| self.stores.iter().map(|s| s.count(t, c)).sum::<u64>();
        (sum(OpClass::Read), sum(OpClass::Write), sum(OpClass::List))
    }
}

async fn send<S: BlobStore>(
    api: &Arc<Api<S>>,
    req: Request<Body>,
) -> (StatusCode, HeaderMap, Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
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

async fn write<S: BlobStore>(api: &Arc<Api<S>>, tenant: u64, id: &str) {
    let body = json!({"durability": "durable", "documents": [
        {"id": id, "vector": [1.0, 0.5], "attributes": {"n": 1}}
    ]});
    let (s, _, b) = send(
        api,
        request(tenant, "PUT", "/v1/indexes/docs/documents", Some(&body)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn relevance(consistency: Option<&str>) -> Value {
    let mut q = json!({"vector": [1.0, 0.5], "top_k": 100, "filters": ["n", "Eq", 1]});
    if let Some(c) = consistency {
        q["consistency"] = json!(c);
    }
    q
}

fn ranked(consistency: Option<&str>) -> Value {
    let mut q = json!({"rank_by": ["id", "asc"], "top_k": 100});
    if let Some(c) = consistency {
        q["consistency"] = json!(c);
    }
    q
}

async fn query<S: BlobStore>(
    api: &Arc<Api<S>>,
    tenant: u64,
    q: &Value,
) -> (StatusCode, HeaderMap, Value) {
    send(
        api,
        request(tenant, "POST", "/v1/indexes/docs/query", Some(q)),
    )
    .await
}

fn ids(b: &Value) -> Vec<String> {
    let mut v: Vec<String> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    v.sort();
    v
}

fn now() -> FoldPolicy {
    FoldPolicy {
        period: Duration::from_secs(1),
        age: Duration::ZERO,
        bytes: 1 << 20,
    }
}

/// Folds only what is requested: nothing is old or large enough.
fn requested_only() -> FoldPolicy {
    FoldPolicy {
        period: Duration::from_secs(1),
        age: Duration::from_secs(3600),
        bytes: 1 << 40,
    }
}

/// Lane A (the first `Api`) holds folded bundles, a watermark above 0, and exactly one
/// unfolded bundle: `x0` folded, `x1` not.
async fn one_unfolded(w: &World, tenant: u64) {
    let a = &w.apis[0];
    write(a, tenant, "x0").await;
    assert_eq!(a.fold_due(&now()).await.folded, 1);
    write(a, tenant, "x1").await;
}

async fn refuses_then_serves(q: fn(Option<&str>) -> Value) {
    let w = world(&[1, 2]);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    one_unfolded(&w, 101).await;
    let (s, h, body) = query(b, 101, &q(Some("strong"))).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "not_folded");
    assert_eq!(body["error"]["retryable"], true);
    assert!(h.contains_key("retry-after"), "{h:?}");
    let (s, _, body) = query(b, 101, &q(None)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ids(&body), ["x0"], "eventual saw the unfolded write");
    // The refusal asked B for a fold of the tenant; nothing else would fold it now.
    let tick = b.fold_due(&requested_only()).await;
    assert_eq!(tick.folded, 1, "{tick:?}");
    let (s, _, body) = query(b, 101, &q(Some("strong"))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(ids(&body), ["x0", "x1"]);
    assert_eq!(body["meta"]["consistency"], "strong");
    let _ = a;
}

#[tokio::test]
async fn strong_refuses_what_it_cannot_see() {
    refuses_then_serves(relevance).await;
}

#[tokio::test]
async fn strong_refuses_for_a_rank_order_too() {
    refuses_then_serves(ranked).await;
}

#[tokio::test]
async fn strong_serves_what_it_can() {
    let w = world(&[1, 2]);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    write(a, 102, "x").await;
    a.fold_due(&now()).await;
    // A lane registered that never wrote: no watermark entry, and nothing to find.
    let view = w.stores[0].as_tenant(TenantId(102));
    pstore_engine::lanes::register(&view, TenantId(102), LaneId(9))
        .await
        .unwrap();
    let (s, _, strong) = query(b, 102, &relevance(Some("strong"))).await;
    assert_eq!(s, StatusCode::OK, "{strong}");
    let (_, _, eventual) = query(b, 102, &relevance(None)).await;
    assert_eq!(strong["results"], eventual["results"]);
    assert_eq!(eventual["meta"]["consistency"], "eventual");
}

#[tokio::test]
async fn the_own_lane_is_probed_past_what_this_process_holds() {
    let w = world(&[1, 2]);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    // Lane B: a watermark of 1 over a folded, unreaped bundle 0.
    write(b, 103, "b").await;
    b.fold_due(&now()).await;
    // Lane A: one unfolded bundle, which A holds in memory.
    write(a, 103, "a").await;
    let before = w.reads(103);
    let (s, _, eventual) = query(a, 103, &relevance(None)).await;
    assert_eq!(s, StatusCode::OK);
    let mid = w.reads(103);
    let (s, _, strong) = query(a, 103, &relevance(Some("strong"))).await;
    assert_eq!(s, StatusCode::OK, "served, not refused: {strong}");
    let after = w.reads(103);
    assert_eq!(ids(&strong), ["a", "b"]);
    assert_eq!(strong["results"], eventual["results"]);
    let e = (mid.0 - before.0, mid.1 - before.1, mid.2 - before.2);
    let st = (after.0 - mid.0, after.1 - mid.1, after.2 - mid.2);
    // One registry GET and one HEAD probe per lane (A and B), and nothing else.
    assert_eq!((st.0, st.1, st.2), (e.0 + 3, e.1, e.2), "{e:?} {st:?}");
}

#[tokio::test]
async fn a_dead_writers_lane_is_not_trusted_after_a_restart() {
    let w = world(&[1]);
    one_unfolded(&w, 104).await;
    // GC has reaped the folded bundle 0: only a probe at the watermark, not at the restarted
    // engine's `next` of 0, can find the unfolded bundle 1 (code review).
    w.switch
        .inner
        .delete_batch(&[Key::new(format!(
            "{:04x}/wal/104/{:016x}/{:016}.bundle",
            104, 1, 0
        ))])
        .await
        .unwrap();
    // A second process on lane 1, holding nothing: a restart.
    let again = Api::new(w.stores[0].clone(), LaneId(1)).unwrap();
    let (s, _, body) = query(&again, 104, &relevance(Some("strong"))).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "not_folded");
}

#[tokio::test]
async fn strong_costs_no_extra_round_trip() {
    let w = world(&[1, 2, 3]);
    for (i, api) in w.apis.iter().enumerate() {
        write(api, 105, &format!("d{i}")).await;
        api.fold_due(&now()).await;
    }
    let a = &w.apis[0];
    for q in [relevance, ranked] {
        w.depth.reset();
        let (s, _, b) = query(a, 105, &q(None)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let eventual = w.depth.depth();
        w.depth.reset();
        let (s, _, b) = query(a, 105, &q(Some("strong"))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // At most: the probes overlap the query's own requests, which the counter can read as
        // fewer rounds, never more. Probes awaited one after another would add three.
        assert!(w.depth.depth() <= eventual, "strong added a round");
    }
}

#[tokio::test]
async fn a_probe_that_fails_is_an_error() {
    let w = world(&[1, 2]);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    write(b, 106, "b").await;
    b.fold_due(&now()).await;
    write(a, 106, "a").await;
    a.fold_due(&now()).await;
    *w.switch.fail.lock().unwrap() = Some(format!("/wal/106/{:016x}/", 2));
    let (s, _, body) = query(a, 106, &relevance(Some("strong"))).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "storage_unavailable");
}

#[tokio::test]
async fn many_refusals_ask_for_one_fold() {
    let w = world(&[1, 2]);
    let b = Arc::clone(&w.apis[1]);
    one_unfolded(&w, 107).await;
    let all = futures_util::future::join_all((0..8).map(|_| {
        let b = Arc::clone(&b);
        async move { query(&b, 107, &relevance(Some("strong"))).await.0 }
    }))
    .await;
    assert!(
        all.iter().all(|s| *s == StatusCode::SERVICE_UNAVAILABLE),
        "{all:?}"
    );
    let first = b.fold_due(&requested_only()).await;
    assert_eq!(first.folded, 1, "{first:?}");
    let second = b.fold_due(&requested_only()).await;
    assert_eq!((second.folded, second.nothing), (0, 0), "{second:?}");
}

#[tokio::test]
async fn what_consistency_cannot_mean_is_refused() {
    let w = world(&[1]);
    let a = &w.apis[0];
    write(a, 108, "x").await;
    for c in [
        json!("Strong"),
        json!("bounded"),
        // M11.1 gave "session" a meaning; its miscased spelling still has none.
        json!("Session"),
        json!(1),
        json!({"mode": "strong"}),
    ] {
        let mut q = relevance(None);
        q["consistency"] = c.clone();
        let (s, _, b) = query(a, 108, &q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{c} was accepted: {b}");
    }
    let mut q = relevance(Some("strong"));
    q["as_of"] = json!(0);
    let (s, _, b) = query(a, 108, &q).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "strong with as_of: {b}");
    let multi =
        json!({"queries": [relevance(None), {"vector": [1.0, 0.5], "consistency": "bounded"}]});
    let (s, _, b) = query(a, 108, &multi).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    // `null` is the default.
    let mut q = relevance(None);
    q["consistency"] = Value::Null;
    let (s, _, b) = query(a, 108, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["meta"]["consistency"], "eventual");
}

#[tokio::test]
async fn each_sub_query_reports_its_level() {
    let w = world(&[1]);
    let a = &w.apis[0];
    write(a, 109, "x").await;
    let multi = json!({"queries": [
        relevance(None),
        relevance(Some("strong")),
        relevance(Some("eventual"))
    ]});
    let (s, _, b) = query(a, 109, &multi).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["meta"]["consistencies"],
        json!(["eventual", "strong", "eventual"])
    );
}

#[test]
fn the_research_is_corrected_where_it_promised_otherwise() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/research");
    let read = |p: &str| std::fs::read_to_string(format!("{root}/{p}")).unwrap();
    assert!(read("03-metadata-consistency/consistency-model.md").contains("Corrected by [M9i.2]"));
    assert!(read("11-design/turbopuffer-api-parity.md").contains("landed in [M11.1]"));
    assert!(read("11-design/api-design.md").contains("`503 not_folded`"));
}
