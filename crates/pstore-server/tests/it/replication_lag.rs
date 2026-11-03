//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A replica's lag is bounded (M192): the worker folds the source every half of the bound,
//! reports the lag, and a client can ask for a sync now.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition,
    PutOutcome,
};
use pstore_server::{Api, ReplicationPolicy, Worker};
use pstore_types::{CasTag, LaneId, TenantId};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

/// A store whose reads of a key containing a pattern fail. Cloned, it is the same store.
#[derive(Debug, Default, Clone)]
struct Switch(Arc<(MemoryStore, Mutex<Option<String>>)>);

impl Switch {
    fn fail(&self, pattern: Option<&str>) {
        *self.0.1.lock().unwrap() = pattern.map(str::to_owned);
    }
    fn gate(&self, key: &Key) -> Result<(), BlobError> {
        let failing = self
            .0
            .1
            .lock()
            .unwrap()
            .as_deref()
            .is_some_and(|p| key.as_str().contains(p));
        if failing {
            Err(BlobError::Other("injected read failure".to_owned()))
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for Switch {
    fn capabilities(&self) -> &Capabilities {
        self.0.0.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.gate(key)?;
        self.0.0.get(key).await
    }
    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.gate(key)?;
        self.0.0.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.gate(key)?;
        self.0.0.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.gate(key)?;
        self.0.0.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.gate(key)?;
        self.0.0.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.gate(key)?;
        self.0.0.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.0.0.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.0.0.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.0.0.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.0.0.list_unrestricted(prefix).await
    }
}

type A = Arc<Api<Switch>>;

const SRC: u64 = 81;
const DST: u64 = 82;
const L: Duration = Duration::from_secs(10);

fn policy(max_lag: Option<Duration>) -> ReplicationPolicy {
    ReplicationPolicy {
        max_lag,
        shards: 4,
        ..ReplicationPolicy::default()
    }
}

/// One store with a server per lane, and a remote store named "far".
struct World {
    switch: Switch,
    acct: Accounted<Switch>,
    far: Accounted<MemoryStore>,
}

impl World {
    fn new() -> Self {
        let switch = Switch::default();
        Self {
            acct: Accounted::new(switch.clone()),
            switch,
            far: Accounted::new(MemoryStore::new()),
        }
    }
    fn api(&self, lane: u64, max_lag: Option<Duration>) -> A {
        let api = Api::unsecured(self.acct.clone(), LaneId(lane)).unwrap();
        let far: Arc<dyn BlobStore> = Arc::new(self.far.as_tenant(TenantId(0)));
        api.configure_replication(policy(max_lag), BTreeMap::from([("far".to_owned(), far)]));
        api
    }
}

async fn send<S: BlobStore + 'static>(
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
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, body)
}

/// Durable rows into the source, left unfolded: only a fold makes them visible elsewhere.
async fn write<S: BlobStore + 'static>(api: &Arc<Api<S>>, ids: std::ops::Range<u32>) {
    let docs: Vec<Value> = ids
        .map(|i| json!({"id": format!("d{i:04}"), "vector": [f64::from(i).sin(), 1.0]}))
        .collect();
    let (s, b) = send(
        api,
        SRC,
        "PUT",
        "/v1/indexes/src/documents",
        &json!({"durability": "durable", "documents": docs}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold<S: BlobStore + 'static>(api: &Arc<Api<S>>) {
    let (s, b) = send(api, SRC, "POST", "/v1/admin/fold", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn rows(api: &A, index: &str) -> Vec<String> {
    let (s, b) = send(
        api,
        DST,
        "POST",
        &format!("/v1/indexes/{index}/query"),
        &json!({"rank_by": ["id", "asc"], "top_k": 10_000}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn status(api: &A, dest: &str) -> Value {
    let (s, b) = send(
        api,
        DST,
        "GET",
        &format!("/v1/indexes/{dest}/replication"),
        &Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

async fn metric(api: &A, line: &str) -> u64 {
    let (s, b) = send(api, DST, "GET", "/metrics", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let text = b.as_str().unwrap().to_owned();
    text.lines()
        .find_map(|l| l.strip_prefix(line)?.trim().parse().ok())
        .unwrap_or_else(|| panic!("no {line} in\n{text}"))
}

/// A source with rows 0..5 folded, a writer on lane 1, and a replica of it created by `worker`.
async fn replicated(world: &World, worker: &A) -> A {
    let writer = world.api(1, None);
    write(&writer, 0..5).await;
    fold(&writer).await;
    let (s, b) = send(
        worker,
        DST,
        "PUT",
        "/v1/indexes/dst/replication",
        &json!({"source": {"index": "src", "tenant": SRC.to_string()}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    writer
}

/// Ticks the worker, then lets one second of paused time pass, `n` times.
async fn run(api: &A, w: &mut Worker, n: usize) {
    for _ in 0..n {
        api.replication_tick(w).await;
        tokio::time::advance(Duration::from_secs(1)).await;
    }
}

/// Seconds, at most `limit`, until the replica holds `id`.
async fn arrives(api: &A, w: &mut Worker, id: &str, limit: usize) -> Option<usize> {
    for s in 0..=limit {
        if rows(api, "dst").await.iter().any(|r| r == id) {
            return Some(s);
        }
        run(api, w, 1).await;
    }
    None
}

#[tokio::test(start_paused = true)]
async fn a_write_reaches_the_replica_within_the_bound() {
    let world = World::new();
    let worker = world.api(2, Some(L));
    let writer = replicated(&world, &worker).await;
    let mut w = Worker::default();
    run(&worker, &mut w, 3).await;
    assert_eq!(rows(&worker, "dst").await.len(), 5);
    // Nobody folds the source but the replica's worker.
    write(&writer, 5..6).await;
    let took = arrives(&worker, &mut w, "d0005", 10).await;
    assert!(
        took.is_some_and(|s| s <= 10),
        "an unfolded write did not reach the replica within the bound: {took:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn without_a_bound_it_waits_for_the_fold() {
    let world = World::new();
    let worker = world.api(2, None);
    let writer = replicated(&world, &worker).await;
    let mut w = Worker::default();
    run(&worker, &mut w, 3).await;
    write(&writer, 5..6).await;
    assert_eq!(arrives(&worker, &mut w, "d0005", 100).await, None);
    assert_eq!(rows(&worker, "dst").await.len(), 5);
}

#[tokio::test(start_paused = true)]
async fn an_idle_source_stays_bounded() {
    let world = World::new();
    let worker = world.api(2, Some(L));
    let writer = replicated(&world, &worker).await;
    let mut w = Worker::default();
    run(&worker, &mut w, 300).await;
    write(&writer, 5..6).await;
    let took = arrives(&worker, &mut w, "d0005", 10).await;
    assert!(took.is_some_and(|s| s <= 10), "{took:?}");
    // Folded every half of the bound, never more often: ~62 in 310 s, nothing to fold in most.
    let folds = metric(
        &worker,
        "pstore_replication_source_fold_total{outcome=\"nothing\"}",
    )
    .await
        + metric(
            &worker,
            "pstore_replication_source_fold_total{outcome=\"folded\"}",
        )
        .await;
    assert!((55..=70).contains(&folds), "{folds} source folds");
}

#[tokio::test(start_paused = true)]
async fn status_reports_the_lag() {
    let world = World::new();
    let worker = world.api(2, Some(L));
    let _writer = replicated(&world, &worker).await;
    let mut w = Worker::default();
    run(&worker, &mut w, 3).await;
    let b = status(&worker, "dst").await;
    assert_eq!(b["max_lag_ms"], 10_000, "{b}");
    assert!(b["fresh_ms"].is_u64(), "{b}");
    let lag = b["lag_ms"].as_u64().unwrap();
    assert!(lag <= 10_000, "{b}");
    assert_eq!(b["lag_exceeded"], false, "{b}");
    // The lag is measured against the fold's start: three seconds on, it reads three more.
    tokio::time::advance(Duration::from_secs(3)).await;
    let later = status(&worker, "dst").await;
    assert_eq!(later["fresh_ms"], b["fresh_ms"], "{later}");
    assert_eq!(later["lag_ms"].as_u64().unwrap(), lag + 3_000, "{later}");
}

#[tokio::test(start_paused = true)]
async fn a_remote_source_is_not_bounded() {
    let world = World::new();
    let far = Api::unsecured(world.far.clone(), LaneId(50)).unwrap();
    write(&far, 0..3).await;
    fold(&far).await;
    let worker = world.api(2, Some(L));
    let (s, b) = send(
        &worker,
        DST,
        "PUT",
        "/v1/indexes/dst/replication",
        &json!({"source": {"index": "src", "tenant": SRC.to_string(), "store": "far"}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let mut w = Worker::default();
    run(&worker, &mut w, 3).await;
    assert_eq!(rows(&worker, "dst").await.len(), 3);
    write(&far, 3..4).await;
    run(&worker, &mut w, 20).await;
    assert_eq!(
        rows(&worker, "dst").await.len(),
        3,
        "a remote source was folded"
    );
    let b = status(&worker, "dst").await;
    assert_eq!(b["max_lag_ms"], Value::Null, "{b}");
    assert_eq!(b["fresh_ms"], Value::Null, "{b}");
    assert_eq!(b["lag_exceeded"], false, "{b}");
    assert_eq!(
        metric(
            &worker,
            "pstore_replication_source_fold_total{outcome=\"folded\"}"
        )
        .await,
        0
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_source_fold_is_exceeded_and_recovers() {
    let world = World::new();
    let worker = world.api(2, Some(L));
    let writer = replicated(&world, &worker).await;
    let mut w = Worker::default();
    run(&worker, &mut w, 3).await;
    write(&writer, 5..6).await;
    world.switch.fail(Some(&format!("/wal/{SRC}/")));
    run(&worker, &mut w, 7).await;
    assert_eq!(status(&worker, "dst").await["lag_exceeded"], false);
    run(&worker, &mut w, 8).await;
    let b = status(&worker, "dst").await;
    assert_eq!(b["lag_exceeded"], true, "{b}");
    assert!(
        b["last_error"].as_str().unwrap().contains("source fold"),
        "{b}"
    );
    assert!(b["lag_ms"].as_u64().unwrap() > 10_000, "{b}");
    assert_eq!(
        metric(&worker, "pstore_replication_lag_exceeded_total").await,
        1
    );
    assert_eq!(metric(&worker, "pstore_replication_lagging").await, 1);
    assert!(
        metric(
            &worker,
            "pstore_replication_source_fold_total{outcome=\"failed\"}"
        )
        .await
            >= 2
    );
    // Readable again: cleared within one fold, and counted once.
    world.switch.fail(None);
    run(&worker, &mut w, 6).await;
    let b = status(&worker, "dst").await;
    assert_eq!(b["lag_exceeded"], false, "{b}");
    assert_eq!(b["last_error"], Value::Null, "{b}");
    assert_eq!(metric(&worker, "pstore_replication_lagging").await, 0);
    assert_eq!(
        metric(&worker, "pstore_replication_lag_exceeded_total").await,
        1
    );
    assert!(rows(&worker, "dst").await.iter().any(|r| r == "d0005"));
}

async fn sync_now(api: &A, dest: &str) -> (StatusCode, Value) {
    send(
        api,
        DST,
        "POST",
        &format!("/v1/indexes/{dest}/replication/sync"),
        &Value::Null,
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn sync_now_folds_the_source_and_syncs() {
    let world = World::new();
    // No worker runs anywhere: only the call moves the replica.
    let control = world.api(3, Some(L));
    let writer = replicated(&world, &control).await;
    write(&writer, 5..8).await;
    let (s, b) = sync_now(&control, "dst").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["source_folded"], true, "{b}");
    assert!(b["fresh_ms"].is_u64(), "{b}");
    assert_eq!(b["index"], "dst", "{b}");
    assert_eq!(rows(&control, "dst").await.len(), 8);
    // Again with nothing new: still answered, and still current.
    let (s, b) = sync_now(&control, "dst").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["fresh_ms"].is_u64(), "{b}");
}

#[tokio::test(start_paused = true)]
async fn sync_now_refuses_by_name() {
    let world = World::new();
    let control = world.api(3, Some(L));
    let _writer = replicated(&world, &control).await;
    let (s, b) = sync_now(&control, "nope").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "replication_not_found", "{b}");
    let (s, b) = send(
        &control,
        DST,
        "POST",
        "/v1/indexes/dst/replication/pause",
        &Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = sync_now(&control, "dst").await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "replication_paused", "{b}");
    // A fold that fails is a 503, and the replica is not touched.
    let (s, _) = send(
        &control,
        DST,
        "POST",
        "/v1/indexes/dst/replication/resume",
        &Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    world.switch.fail(Some(&format!("/wal/{SRC}/")));
    let (s, b) = sync_now(&control, "dst").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("source fold"),
        "{b}"
    );
}

#[tokio::test(start_paused = true)]
async fn sync_now_over_a_remote_source_syncs_without_folding() {
    let world = World::new();
    let far = Api::unsecured(world.far.clone(), LaneId(50)).unwrap();
    write(&far, 0..3).await;
    fold(&far).await;
    let control = world.api(3, Some(L));
    let (s, b) = send(
        &control,
        DST,
        "PUT",
        "/v1/indexes/dst/replication",
        &json!({"source": {"index": "src", "tenant": SRC.to_string(), "store": "far"}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    write(&far, 3..4).await;
    let (s, b) = sync_now(&control, "dst").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["source_folded"], false, "{b}");
    assert_eq!(b["fresh_ms"], Value::Null, "{b}");
    // What the source had folded, and not the row it had not.
    assert_eq!(rows(&control, "dst").await.len(), 3);
}

#[tokio::test(start_paused = true)]
async fn sync_now_finishes_a_copy() {
    let world = World::new();
    let control = world.api(3, Some(L));
    let writer = world.api(1, None);
    write(&writer, 0..4).await;
    fold(&writer).await;
    let (s, b) = send(
        &control,
        DST,
        "POST",
        "/v1/indexes/dst/copy",
        &json!({"source": {"index": "src", "tenant": SRC.to_string()}}),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    write(&writer, 4..6).await;
    let (s, b) = sync_now(&control, "dst").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["state"], "succeeded", "{b}");
    assert_eq!(b["source_folded"], true, "{b}");
    assert_eq!(rows(&control, "dst").await.len(), 6);
}

#[test]
fn replication_max_lag_is_validated() {
    use pstore_server::{Config, ConfigError};
    let vars = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .chain(&[("PSTORE_LANE", "1")])
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    };
    let none = Config::from_vars(vars(&[])).unwrap();
    assert_eq!(none.replication.unwrap().max_lag, None);
    let set = Config::from_vars(vars(&[("PSTORE_REPLICATION_MAX_LAG_S", "2")])).unwrap();
    assert_eq!(
        set.replication.unwrap().max_lag,
        Some(Duration::from_secs(2))
    );
    for v in ["1", "0", "x"] {
        let pairs: &'static [(&'static str, &'static str)] = match v {
            "1" => &[("PSTORE_REPLICATION_MAX_LAG_S", "1")],
            "0" => &[("PSTORE_REPLICATION_MAX_LAG_S", "0")],
            _ => &[("PSTORE_REPLICATION_MAX_LAG_S", "x")],
        };
        assert!(
            matches!(
                Config::from_vars(vars(pairs)),
                Err(ConfigError::Replication("PSTORE_REPLICATION_MAX_LAG_S", ..))
            ),
            "{v} was accepted"
        );
    }
}
