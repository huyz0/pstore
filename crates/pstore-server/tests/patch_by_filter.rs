//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `delete_by_filter` and `patch_by_filter` (M13.2): one deferred operation each, applied at
//! the fold to every row whose current version the filter admits.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

fn apis(lanes: &[u64]) -> Vec<A> {
    let store = Accounted::new(MemoryStore::new());
    lanes
        .iter()
        .map(|l| Api::new(store.clone(), LaneId(*l)).unwrap())
        .collect()
}

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "43")
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

async fn write(api: &A, mut body: Value) {
    body["durability"] = json!("durable");
    let (s, b) = send(api, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn rows(api: &A) -> BTreeMap<String, Value> {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 10_000, "include_attributes": true});
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                r["attributes"].clone(),
            )
        })
        .collect()
}

fn doc(i: usize) -> Value {
    json!({"id": format!("d{i:05}"), "vector": [1.0, 0.5], "attributes": {"n": i, "t": i % 3}})
}

/// 2,000 rows in 4 folds; returns the model.
async fn seeded(a: &A) -> BTreeMap<String, Value> {
    for k in 0..4 {
        let docs: Vec<Value> = (k * 500..(k + 1) * 500).map(doc).collect();
        write(a, json!({"documents": docs})).await;
        fold(a).await;
    }
    rows(a).await
}

#[tokio::test]
async fn a_delete_by_filter_deletes_exactly_what_it_admits() {
    let w = apis(&[1]);
    let a = &w[0];
    let mut model = seeded(a).await;
    assert_eq!(model.len(), 2000);
    write(
        a,
        json!({"delete_by_filter": ["And", [["n", "Lt", 700], ["t", "Eq", 1]]]}),
    )
    .await;
    fold(a).await;
    model.retain(|_, attrs| !(attrs["n"].as_u64().unwrap() < 700 && attrs["t"] == 1));
    assert_eq!(rows(a).await, model);
}

#[tokio::test]
async fn a_patch_by_filter_merges_into_exactly_what_it_admits() {
    let w = apis(&[1]);
    let a = &w[0];
    let mut model = seeded(a).await;
    write(
        a,
        json!({"patch_by_filter": {"filters": ["t", "Eq", 2], "attributes": {"tag": "two", "t": null}}}),
    )
    .await;
    fold(a).await;
    for attrs in model.values_mut() {
        let o: &mut Map<String, Value> = attrs.as_object_mut().unwrap();
        if o["t"] == 2 {
            o.remove("t");
            o.insert("tag".into(), json!("two"));
        }
    }
    assert_eq!(rows(a).await, model);
}

#[tokio::test]
async fn a_by_filter_operation_sees_this_folds_earlier_operations() {
    let w = apis(&[1]);
    let a = &w[0];
    write(a, json!({"documents": [doc(1), doc(2)]})).await;
    fold(a).await;
    // In one fold: a new row that matches, a folded row patched out of matching, then the
    // delete by filter.
    write(
        a,
        json!({"documents": [{"id": "new", "vector": [1.0, 0.5], "attributes": {"n": 5}}]}),
    )
    .await;
    write(
        a,
        json!({"patch_rows": [{"id": "d00002", "attributes": {"n": 500}}]}),
    )
    .await;
    write(a, json!({"delete_by_filter": ["n", "Lt", 100]})).await;
    fold(a).await;
    let got = rows(a).await;
    assert_eq!(got.keys().collect::<Vec<_>>(), ["d00002"], "{got:?}");
}

#[tokio::test]
async fn a_by_filter_operation_is_invisible_until_its_fold() {
    let w = apis(&[1, 2]);
    let (a, b) = (&w[0], &w[1]);
    write(a, json!({"documents": [doc(1), doc(2)]})).await;
    fold(a).await;
    write(a, json!({"delete_by_filter": ["n", "Lt", 100]})).await;
    for api in [a, b] {
        assert_eq!(
            rows(api).await.len(),
            2,
            "a delete by filter showed before its fold"
        );
    }
    fold(a).await;
    assert!(rows(b).await.is_empty());
}

#[tokio::test]
async fn what_a_by_filter_operation_cannot_mean_is_refused() {
    let w = apis(&[1]);
    let a = &w[0];
    write(a, json!({"documents": [doc(1)]})).await;
    for (body, why) in [
        (
            json!({"durability": "durable", "delete_by_filter": "n"}),
            "filter",
        ),
        (
            json!({"durability": "durable", "patch_by_filter": {"filters": ["n", "Eq", 1]}}),
            "patch_by_filter",
        ),
        (
            json!({"durability": "durable", "patch_by_filter": {"attributes": {"a": 1}}}),
            "patch_by_filter",
        ),
        (
            json!({"durability": "durable", "patch_by_filter": {"filters": ["n", "Eq", 1], "attributes": {"a": 1}, "x": 1}}),
            "patch_by_filter",
        ),
        (
            json!({"durability": "batched", "delete_by_filter": ["n", "Eq", 1]}),
            "durable",
        ),
    ] {
        let (s, b) = send(a, "PUT", "/v1/indexes/docs/documents", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} was accepted: {b}");
        let msg = b["error"]["message"].as_str().unwrap();
        assert!(msg.contains(why), "{body}: {msg:?} does not name {why:?}");
    }
}
