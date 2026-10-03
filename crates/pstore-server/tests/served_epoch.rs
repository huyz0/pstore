//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `meta.epoch` is the epoch the answer was served from, not this process's last commit (M10,
//! BACKLOG row 40).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

async fn send(api: &Arc<Api<MemoryStore>>, req: Request<Body>) -> (StatusCode, Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn request(method: &str, uri: &str, body: Option<&Value>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT)
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

const TENANT: &str = "19";

async fn put(api: &Arc<Api<MemoryStore>>, id: &str, n: i64) {
    let body = json!({"durability": "durable", "documents": [
        {"id": id, "vector": [n as f64, 0.5], "attributes": {"n": n}}]});
    let (s, b) = send(
        api,
        request("PUT", "/v1/indexes/docs/documents", Some(&body)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Folds, and returns the epoch the fold committed.
async fn fold(api: &Arc<Api<MemoryStore>>) -> u64 {
    let (s, b) = send(api, request("POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["epoch"].as_u64().unwrap()
}

async fn query(api: &Arc<Api<MemoryStore>>, q: &Value) -> Value {
    let (s, b) = send(api, request("POST", "/v1/indexes/docs/query", Some(q))).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b
}

fn relevance() -> Value {
    json!({"vector": [1.0, 0.5], "top_k": 10})
}

fn ordered() -> Value {
    json!({"rank_by": ["n", "asc"], "top_k": 10})
}

fn ids(b: &Value) -> Vec<String> {
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn two() -> (Arc<Api<MemoryStore>>, Arc<Api<MemoryStore>>) {
    let store = Accounted::new(MemoryStore::new());
    (
        Api::new(store.clone(), LaneId(1)).unwrap(),
        Api::new(store, LaneId(2)).unwrap(),
    )
}

#[tokio::test]
async fn a_read_only_process_reports_the_epoch_it_served() {
    let (a, b) = two();
    put(&a, "x", 1).await;
    let e = fold(&a).await;
    assert!(e >= 1);
    let mut strong = relevance();
    strong["consistency"] = json!("strong");
    let mut costs = Vec::new();
    for q in [relevance(), ordered(), strong.clone()] {
        let got = query(&b, &q).await;
        costs.push(got["meta"]["cost"].clone());
    }
    // Criterion 5, pinned as measured BEFORE M10: the epoch comes from a HEAD the query
    // already read, so reporting it costs nothing. M27 removed one read from each vector
    // query: the 404 for the small segment's centroid table, which HEAD's count says is absent.
    // M32 adds 37 bytes to the HEAD each reads (the optional sections' zero counts and the
    // dictionaries section), and no read. M45 adds 18 more, its lengths section: a count, the
    // name `docs`, a count, and the one segment's 2-byte length.
    let reads: Vec<_> = costs.iter().map(|c| c["blob_reads"].clone()).collect();
    let bytes: Vec<_> = costs.iter().map(|c| c["bytes_read"].clone()).collect();
    assert_eq!(json!(reads), json!([4, 3, 6]));
    assert_eq!(json!(bytes), json!([582 + 18, 574 + 18, 590 + 18]));
    assert!(costs.iter().all(|c| c["blob_lists"] == 0));
    for q in [relevance(), ordered(), strong] {
        assert_eq!(query(&b, &q).await["meta"]["epoch"], e, "{q}");
    }
    let multi = query(&b, &json!({"queries": [relevance(), ordered()]})).await;
    assert_eq!(multi["meta"]["epochs"], json!([e, e]));
}

#[tokio::test]
async fn a_process_behind_another_reports_the_newer_epoch_it_served() {
    let (a, b) = two();
    put(&a, "x", 1).await;
    let e = fold(&a).await;
    put(&b, "y", 2).await;
    assert_eq!(fold(&b).await, e + 1);
    for q in [relevance(), ordered()] {
        let got = query(&a, &q).await;
        assert_eq!(got["meta"]["epoch"], e + 1, "{q}");
        assert_eq!(ids(&got).len(), 2, "{q}");
    }
}

#[tokio::test]
async fn the_served_epoch_repeats_the_read_as_of() {
    // Distinct vectors and distinct `n`, so neither order rests on a tie-break.
    let (a, b) = two();
    put(&a, "x", 1).await;
    put(&a, "y", 2).await;
    fold(&a).await;
    for q in [relevance(), ordered()] {
        let live = query(&b, &q).await;
        let mut again = q.clone();
        again["as_of"] = live["meta"]["epoch"].clone();
        let past = query(&b, &again).await;
        assert_eq!(ids(&past), ids(&live), "{q}");
        assert_eq!(ids(&live).len(), 2, "{q}");
        assert_eq!(past["meta"]["epoch"], live["meta"]["epoch"]);
    }
}

/// A batched write to `fresh`: held in this process's memory only, never folded.
async fn hold_fresh(api: &Arc<Api<MemoryStore>>) {
    let body = json!({"durability": "batched", "documents": [
        {"id": "f", "vector": [1.0, 0.5]}]});
    let (s, b) = send(
        api,
        request("PUT", "/v1/indexes/fresh/documents", Some(&body)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn summary(api: &Arc<Api<MemoryStore>>) -> Value {
    let (s, b) = send(api, request("GET", "/v1/indexes/fresh", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["unfolded"], true, "{b}");
    b
}

#[tokio::test]
async fn an_unfolded_index_reports_the_epoch_of_the_head_read() {
    // M10.2, BACKLOG row 42: a process that never committed.
    let (a, b) = two();
    put(&b, "x", 1).await;
    let e = fold(&b).await;
    assert!(e >= 1);
    hold_fresh(&a).await;
    let got = summary(&a).await;
    assert_eq!(got["epoch"], e, "{got}");
    // One HEAD read, as before the fix.
    assert_eq!(got["cost"]["blob_reads"], 1, "{got}");
    assert_eq!(got["cost"]["blob_writes"], 0);
    assert_eq!(got["cost"]["blob_lists"], 0);
}

#[tokio::test]
async fn an_unfolded_index_on_a_stale_process_reports_the_newer_epoch() {
    let (a, b) = two();
    put(&a, "x", 1).await;
    let e = fold(&a).await;
    put(&b, "y", 2).await;
    assert_eq!(fold(&b).await, e + 1);
    hold_fresh(&a).await;
    assert_eq!(summary(&a).await["epoch"], e + 1);
}
