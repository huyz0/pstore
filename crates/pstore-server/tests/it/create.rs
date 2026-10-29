//! M147: `PUT /v1/indexes/{index}` creates an index with no rows.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_cache::CacheCore;
use pstore_server::{Api, Auth, FoldPolicy, RequestLimits};
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const TENANT: u64 = 41;

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT.to_string())
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn cached(store: &Accounted<MemoryStore>, lane: u64) -> A {
    Api::with_cache_unsecured(
        store.clone(),
        LaneId(lane),
        Arc::new(CacheCore::memory(16 << 20)),
    )
    .unwrap()
}

async fn create(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    send(api, "PUT", &format!("/v1/indexes/{index}"), body).await
}

async fn write(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    send(api, "PUT", &format!("/v1/indexes/{index}/documents"), body).await
}

async fn query(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    send(api, "POST", &format!("/v1/indexes/{index}/query"), body).await
}

async fn fold(api: &A) {
    api.fold_due(&FoldPolicy {
        period: std::time::Duration::from_secs(1),
        age: std::time::Duration::ZERO,
        bytes: 1,
    })
    .await;
}

fn puts(store: &Accounted<MemoryStore>) -> u64 {
    store.count(TenantId(u128::from(TENANT)), OpClass::Write)
}

fn listed(b: &Value) -> Vec<String> {
    b["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap().to_owned())
        .collect()
}

/// Every read route answers an empty index as existing and empty.
async fn reads_as_empty(api: &A, index: &str, dims: u64, metric: &str) {
    let (s, b) = send(api, "GET", "/v1/indexes", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(listed(&b).contains(&index.to_owned()), "{b}");
    let (s, b) = send(api, "GET", &format!("/v1/indexes/{index}"), &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["segments"].as_u64(), b["documents"].as_u64()),
        (Some(0), Some(0)),
        "{b}"
    );
    assert_eq!(b["schema"]["dims"], dims, "{b}");
    assert_eq!(b["schema"]["distance_metric"], metric, "{b}");
    let vector: Vec<f64> = (0..dims).map(|_| 1.0).collect();
    let (s, b) = query(api, index, &json!({"vector": vector, "top_k": 5})).await;
    assert_eq!(s, StatusCode::OK, "relevance: {b}");
    assert_eq!(b["results"], json!([]), "{b}");
    let (s, b) = query(api, index, &json!({"rank_by": ["n", "asc"], "top_k": 5})).await;
    assert_eq!(s, StatusCode::OK, "rank_by: {b}");
    assert_eq!(b["results"], json!([]), "{b}");
    let (s, b) = query(
        api,
        index,
        &json!({"aggregate_by": {"n": ["Count", "id"]}, "top_k": 5}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "aggregate: {b}");
    assert_eq!(b["aggregations"]["n"], 0, "aggregate: {b}");
    let (s, b) = send(
        api,
        "POST",
        &format!("/v1/indexes/{index}/warm"),
        &Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "warm: {b}");
    assert_eq!(b["segments"], 0, "{b}");
}

#[tokio::test]
async fn created_index_survives_restart() {
    let store = Accounted::new(MemoryStore::new());
    let api = cached(&store, 1);
    let (s, b) = create(
        &api,
        "fresh",
        &json!({"dims": 3, "distance_metric": "cosine_distance"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let epoch = b["epoch"].as_u64().unwrap();
    assert!(epoch > 0, "{b}");
    reads_as_empty(&api, "fresh", 3, "cosine_distance").await;
    // A restart: another process over the same store.
    let again = cached(&store, 2);
    reads_as_empty(&again, "fresh", 3, "cosine_distance").await;
    // Absent at its creation epoch: `as_of` reads only an index with a segment.
    let (s, b) = query(
        &again,
        "fresh",
        &json!({"vector": [1, 1, 1], "top_k": 1, "as_of": epoch}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn create_refusals_touch_nothing() {
    let store = Accounted::new(MemoryStore::new());
    let limits = RequestLimits::builder().array_elements(64).build().unwrap();
    let api = Api::with_limits(
        store.clone(),
        LaneId(1),
        None,
        Auth::from_vars(|k| (k == "PSTORE_ALLOW_UNAUTHENTICATED").then(|| "1".to_owned())).unwrap(),
        limits,
    )
    .unwrap();
    // Each kind of existing name: segments, rows held unfolded, rejects only.
    let (s, b) = write(
        &api,
        "folded",
        &json!({"durability": "durable",
        "documents": [{"id": "a", "vector": [1.0]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&api).await;
    let (s, b) = write(
        &api,
        "held",
        &json!({"documents": [{"id": "a", "vector": [1.0]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = create(&api, "made", &json!({"dims": 1})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let before = puts(&store);
    for (index, body, status, code) in [
        (
            "folded",
            json!({"dims": 1}),
            StatusCode::CONFLICT,
            "index_exists",
        ),
        (
            "held",
            json!({"dims": 1}),
            StatusCode::CONFLICT,
            "index_exists",
        ),
        (
            "made",
            json!({"dims": 2}),
            StatusCode::CONFLICT,
            "index_exists",
        ),
        (
            "x",
            json!({"dims": 0}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "x",
            json!({"dims": 65}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "x",
            json!({"dims": -1}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        ("x", json!({}), StatusCode::BAD_REQUEST, "bad_request"),
        (
            "x",
            json!({"dims": 2, "distance_metric": "manhattan"}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "x",
            json!({"dims": 2, "text_field": "t"}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        ("x", json!([2]), StatusCode::BAD_REQUEST, "bad_request"),
        (
            "..",
            json!({"dims": 2}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
    ] {
        let (s, b) = create(&api, index, &body).await;
        assert_eq!(s, status, "{index} {body}: {b}");
        assert_eq!(b["error"]["code"], code, "{index} {body}: {b}");
    }
    assert_eq!(puts(&store), before, "a refusal wrote");
    // At the limit is admitted.
    let (s, b) = create(&api, "wide", &json!({"dims": 64})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn first_write_follows_the_declared_schema() {
    let store = Accounted::new(MemoryStore::new());
    let api = cached(&store, 1);
    let (s, b) = create(
        &api,
        "c",
        &json!({"dims": 2, "distance_metric": "cosine_distance"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let before = puts(&store);
    // No read between the create and these: this process's own caches must know.
    for (body, why, code) in [
        (
            json!({"distance_metric": "cosine_distance",
            "documents": [{"id": "a", "vector": [1.0, 0.0, 0.0]}]}),
            "another width",
            "schema_conflict",
        ),
        (
            json!({"documents": [{"id": "a", "vector": [1.0, 0.0]}]}),
            "the default metric",
            "schema_conflict",
        ),
        (
            json!({"distance_metric": "cosine_distance", "schema": {"text": {"type": "string",
            "full_text_search": {"language": "french"}}},
            "documents": [{"id": "a", "vector": [1.0, 0.0], "text": "bonjour"}]}),
            "another analyzer",
            "schema_conflict",
        ),
    ] {
        let (s, b) = write(&api, "c", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{why}: {b}");
        assert_eq!(b["error"]["code"], code, "{why}: {b}");
    }
    assert_eq!(puts(&store), before, "a refused write wrote");
    let (s, b) = write(
        &api,
        "c",
        &json!({"durability": "durable", "distance_metric": "cosine_distance",
        "documents": [{"id": "a", "vector": [1.0, 0.0]}, {"id": "b", "vector": [0.0, 1.0]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&api).await;
    let (s, b) = send(&api, "GET", "/v1/indexes/c", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["segments"].as_u64(), b["documents"].as_u64()),
        (Some(1), Some(2)),
        "{b}"
    );
    let (s, b) = query(&api, "c", &json!({"vector": [3.0, 0.0], "top_k": 2})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // Cosine: 1 - cos, so 0 for the parallel row and 1 for the orthogonal one, whatever the
    // query's length; a dot product would score them -3 and 0.
    let r = &b["results"];
    assert_eq!(r[0]["id"], "a", "{b}");
    assert!(r[0]["$dist"].as_f64().unwrap().abs() < 1e-5, "{b}");
    assert!((r[1]["$dist"].as_f64().unwrap() - 1.0).abs() < 1e-5, "{b}");
}

#[tokio::test]
async fn a_fold_keeps_an_empty_index() {
    let store = Accounted::new(MemoryStore::new());
    let api = cached(&store, 1);
    let (s, b) = create(&api, "empty", &json!({"dims": 2})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = write(
        &api,
        "other",
        &json!({"durability": "durable",
        "documents": [{"id": "a", "vector": [1.0]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&api).await;
    reads_as_empty(&cached(&store, 2), "empty", 2, "dot_product").await;
}

#[tokio::test]
async fn an_empty_index_drops_and_its_name_is_reused() {
    let store = Accounted::new(MemoryStore::new());
    let api = cached(&store, 1);
    let (s, b) = create(&api, "re", &json!({"dims": 2})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(&api, "DELETE", "/v1/indexes/re", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(&api, "GET", "/v1/indexes", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(!listed(&b).contains(&"re".to_owned()), "{b}");
    let (s, b) = send(&api, "GET", "/v1/indexes/re", &Value::Null).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    let (s, b) = create(&api, "re", &json!({"dims": 3})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // Sent next, in the same process: the old width is refused, the new one admitted.
    let (s, b) = write(
        &api,
        "re",
        &json!({"documents": [{"id": "a", "vector": [1.0, 2.0]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, b) = write(
        &api,
        "re",
        &json!({"documents": [{"id": "a", "vector": [1.0, 2.0, 3.0]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn create_costs_one_read_and_one_cas() {
    let store = Accounted::new(MemoryStore::new());
    let t = TenantId(u128::from(TENANT));
    let before = [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(t, c));
    let (s, b) = create(&cached(&store, 1), "one", &json!({"dims": 4})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let after = [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(t, c));
    assert_eq!(
        [
            after[0] - before[0],
            after[1] - before[1],
            after[2] - before[2]
        ],
        [1, 1, 0],
        "HEAD GET, HEAD CAS, no LIST"
    );
    assert_eq!(b["cost"]["blob_reads"], 1, "{b}");
    assert_eq!(b["cost"]["blob_writes"], 1, "{b}");
}
