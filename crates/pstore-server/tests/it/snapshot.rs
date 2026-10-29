//! One snapshot for every leg of a request (M131): a multi-query's sub-queries, and a ranked
//! query's facets, answer from one HEAD whatever folds while they run.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition,
    PutOutcome,
};
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_server::Api;
use pstore_types::{CasTag, LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tower::ServiceExt;

const T: TenantId = TenantId(31);

/// As the engine's `snapshot.rs`: counts HEAD and lane-registry reads; when armed, the next
/// HEAD read takes its bytes, lets a rival fold and commit on the unwrapped store, and returns
/// the bytes it took, while every other HEAD read waits for that commit.
struct Inner {
    store: Arc<MemoryStore>,
    rival: Engine<MemoryStore>,
    armed: AtomicBool,
    folding: AtomicBool,
    head_reads: AtomicU64,
    lane_reads: AtomicU64,
}

#[derive(Clone)]
struct Interfering(Arc<Inner>);

impl Interfering {
    fn reads(&self) -> (u64, u64) {
        (
            self.0.head_reads.load(Ordering::SeqCst),
            self.0.lane_reads.load(Ordering::SeqCst),
        )
    }
}

#[async_trait::async_trait]
impl BlobStore for Interfering {
    fn capabilities(&self) -> &Capabilities {
        self.0.store.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        if key.as_str().ends_with("/lanes") {
            self.0.lane_reads.fetch_add(1, Ordering::SeqCst);
        }
        self.0.store.get(key).await
    }
    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.0.store.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.0.store.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        let s = &self.0;
        if !key.as_str().ends_with("/HEAD") {
            return s.store.get_with_tag(key).await;
        }
        s.head_reads.fetch_add(1, Ordering::SeqCst);
        if s.armed.swap(false, Ordering::SeqCst) {
            s.folding.store(true, Ordering::SeqCst);
            let before = s.store.get_with_tag(key).await;
            s.rival.fold().await.unwrap();
            s.folding.store(false, Ordering::SeqCst);
            return before;
        }
        while s.folding.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        s.store.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.0.store.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.0.store.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.0.store.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.0.store.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.0.store.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.0.store.list_unrestricted(prefix).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: Class,
    ) -> Result<Bytes, BlobError> {
        self.0.store.get_range_as(key, range, class).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        self.0.store.get_suffix_as(key, n, class).await
    }
}

fn row(id: &str) -> Document {
    let mut d = Document::new(id, vec![1.0, 0.5]);
    d.attrs.insert("n".to_owned(), pstore_format::Value::Int(1));
    d
}

/// Folded `a` and `b` at epoch 1; the rival's `c` flushed, not folded; a reader on lane 2.
async fn world() -> (Interfering, Arc<Api<Interfering>>) {
    let inner = Arc::new(MemoryStore::new());
    let w = Engine::new(Arc::clone(&inner), T, LaneId(1));
    w.write("docs", vec![row("a"), row("b")]).await.unwrap();
    w.flush().await.unwrap();
    w.fold().await.unwrap();
    let rival = Engine::new(Arc::clone(&inner), T, LaneId(9));
    rival.write("docs", vec![row("c")]).await.unwrap();
    rival.flush().await.unwrap();
    let store = Interfering(Arc::new(Inner {
        store: inner,
        rival,
        armed: AtomicBool::new(false),
        folding: AtomicBool::new(false),
        head_reads: AtomicU64::new(0),
        lane_reads: AtomicU64::new(0),
    }));
    let api = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
    (store, api)
}

async fn query(api: &Arc<Api<Interfering>>, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/v1/indexes/docs/query")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "31")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// Two ranked legs and an ordered one, at `level`.
fn three(level: &str) -> Value {
    json!({"queries": [
        {"vector": [1.0, 0.5], "top_k": 10, "consistency": level},
        {"vector": [1.0, 0.5], "top_k": 10, "filters": ["n", "Eq", 1], "consistency": level},
        {"rank_by": ["id", "asc"], "top_k": 10, "consistency": level},
    ]})
}

/// Criterion 1: every leg from one epoch, and the row folded meanwhile in none.
#[tokio::test]
async fn legs_share_one_snapshot() {
    let (store, api) = world().await;
    store.0.armed.store(true, Ordering::SeqCst);
    let (s, b) = query(&api, &three("eventual")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["meta"]["epochs"], json!([1, 1, 1]), "{b}");
    assert!(!b["results"].to_string().contains("\"c\""), "{b}");
    assert_eq!(b["results"][2].as_array().unwrap().len(), 2, "{b}");
    // The rival did fold: the next request sees it.
    let (_, b) = query(&api, &three("eventual")).await;
    assert_eq!(b["meta"]["epochs"], json!([2, 2, 2]), "{b}");
}

/// Criterion 2: the facets count what the hits were drawn from.
#[tokio::test]
async fn facets_share_the_ranked_snapshot() {
    let (store, api) = world().await;
    store.0.armed.store(true, Ordering::SeqCst);
    let q = json!({"vector": [1.0, 0.5], "top_k": 10,
                   "facets": {"aggregate_by": {"n": ["Count", "id"]}}});
    let (s, b) = query(&api, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["meta"]["epoch"], 1, "{b}");
    assert_eq!(b["meta"]["facets_epoch"], 1, "{b}");
    assert_eq!(b["facets"]["n"], 2, "{b}");
    assert!(!b["results"].to_string().contains("\"c\""), "{b}");
}

/// Criterion 3: one HEAD read for the request, and one lane-registry read when strong.
#[tokio::test]
async fn a_pinned_request_reads_head_once() {
    for level in ["eventual", "strong"] {
        let (store, api) = world().await;
        store.0.rival.fold().await.unwrap();
        let before = store.reads();
        let (s, b) = query(&api, &three(level)).await;
        assert_eq!(s, StatusCode::OK, "{level}: {b}");
        let (heads, lanes) = store.reads();
        assert_eq!(heads - before.0, 1, "{level}: {b}");
        assert_eq!(
            lanes - before.1,
            u64::from(level == "strong"),
            "{level}: {b}"
        );
    }
}

/// Criterion 4: legs at two levels run as before, each at its own.
#[tokio::test]
async fn mixed_levels_are_unpinned() {
    let (_store, api) = world().await;
    let q = json!({"queries": [
        {"vector": [1.0, 0.5], "top_k": 10, "consistency": "eventual"},
        {"vector": [1.0, 0.5], "top_k": 10, "consistency": "strong"},
    ]});
    // The rival's `c` is durable and unfolded: the strong leg refuses the request.
    let (s, b) = query(&api, &q).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["code"], "not_folded", "{b}");
}

/// M164 criterion 3: a multi-query's aggregation and facets count what its hits were drawn
/// from, whatever folds while they run.
#[tokio::test]
async fn aggregations_and_facets_share_the_legs_snapshot() {
    let (store, api) = world().await;
    store.0.armed.store(true, Ordering::SeqCst);
    let count = json!({"n": ["Count", "id"]});
    let q = json!({"queries": [
        {"vector": [1.0, 0.5], "top_k": 10},
        {"aggregate_by": count},
        {"vector": [1.0, 0.5], "top_k": 10, "facets": {"aggregate_by": count}},
    ]});
    let (s, b) = query(&api, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["meta"]["epochs"], json!([1, 1, 1]), "{b}");
    assert_eq!(b["meta"]["facets_epochs"], json!([null, null, 1]), "{b}");
    assert_eq!(b["aggregations"][1]["n"], 2, "{b}");
    assert_eq!(b["facets"][2]["n"], 2, "{b}");
    assert!(!b["results"].to_string().contains("\"c\""), "{b}");
    // The rival did fold: the next request counts its row.
    let (_, b) = query(&api, &q).await;
    assert_eq!(b["aggregations"][1]["n"], 3, "{b}");
}

/// M164 criterion 8: a request that cannot be pinned as a whole still answers a faceted
/// sub-query's hits and facets from one snapshot, as that query alone is answered.
#[tokio::test]
async fn an_unpinned_multi_query_pins_each_faceted_pair() {
    let (store, api) = world().await;
    store.0.armed.store(true, Ordering::SeqCst);
    let q = json!({"queries": [
        {"vector": [1.0, 0.5], "top_k": 10, "facets": {"aggregate_by": {"n": ["Count", "id"]}}},
        {"vector": [1.0, 0.5], "top_k": 10, "as_of": 1},
    ]});
    let (s, b) = query(&api, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["meta"]["facets_epochs"][0], b["meta"]["epochs"][0], "{b}");
    let hits = b["results"][0].as_array().unwrap().len();
    assert_eq!(b["facets"][0]["n"], hits, "{b}");
    // Whichever leg takes the armed read, the pair is one epoch's: 1 with 2 rows, 2 with 3.
    let n = b["facets"][0]["n"].as_u64().unwrap();
    assert_eq!(b["meta"]["epochs"][0].as_u64().unwrap() + 1, n, "{b}");
}
