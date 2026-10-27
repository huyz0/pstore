//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Order by up to eight attributes through the API — M140.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_server::Api;
use pstore_types::{LaneId, TenantId};
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
        .header("x-pstore-tenant", "15")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn query(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(api, request("POST", "/v1/indexes/docs/query", Some(body))).await
}

async fn ids(api: &Arc<Api<MemoryStore>>, body: Value) -> Vec<String> {
    let (s, b) = query(api, &body).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

/// `a` and `b`, either absent when `null`.
fn doc(id: &str, a: Value, b: Value) -> Value {
    let mut d = json!({"id": id, "vector": [1.0, 0.5], "attributes": {}});
    for (k, v) in [("a", a), ("b", b)] {
        if !v.is_null() {
            d["attributes"][k] = v;
        }
    }
    d
}

/// Six rows, half folded into a segment and half unfolded.
async fn loaded(store: Accounted<MemoryStore>) -> Arc<Api<MemoryStore>> {
    let api = Api::unsecured(store, LaneId(1)).unwrap();
    let n = Value::Null;
    let docs = [
        doc("r1", json!(1), json!("x")),
        doc("r2", json!(1), json!("y")),
        doc("r3", json!(2), n.clone()),
        doc("r4", json!(2), json!("x")),
        doc("r5", n, json!("z")),
        doc("r6", json!(1), Value::Null),
    ];
    let (s, b) = send(
        &api,
        request(
            "PUT",
            "/v1/indexes/docs/documents",
            Some(&json!({"durability": "durable", "documents": docs[..3]})),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(&api, request("POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(
        &api,
        request(
            "PUT",
            "/v1/indexes/docs/documents",
            Some(&json!({"documents": docs[3..]})),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    api
}

#[tokio::test]
async fn two_keys_order_by_the_first_then_the_second_then_the_id() {
    let api = loaded(Accounted::new(MemoryStore::new())).await;
    let by = json!([["a", "asc"], ["b", "desc"]]);
    assert_eq!(
        ids(&api, json!({"rank_by": by, "top_k": 100})).await,
        ["r2", "r1", "r6", "r4", "r3", "r5"]
    );
    assert_eq!(
        ids(&api, json!({"rank_by": by, "top_k": 3, "offset": 2})).await,
        ["r6", "r4", "r3"]
    );
    // The other directions: `b` ascending puts r1 before r2; `a` descending puts 2 first.
    assert_eq!(
        ids(
            &api,
            json!({"rank_by": [["a", "desc"], ["b", "asc"]], "top_k": 100})
        )
        .await,
        ["r4", "r3", "r1", "r2", "r6", "r5"]
    );
    // `id` as the last key, descending.
    assert_eq!(
        ids(
            &api,
            json!({"rank_by": [["a", "asc"], ["id", "desc"]], "top_k": 100})
        )
        .await,
        ["r6", "r2", "r1", "r4", "r3", "r5"]
    );
}

#[tokio::test]
async fn one_key_in_a_list_answers_as_one_pair_does() {
    let api = loaded(Accounted::new(MemoryStore::new())).await;
    for dir in ["asc", "desc"] {
        let pair = ids(&api, json!({"rank_by": ["b", dir], "top_k": 100})).await;
        let list = ids(&api, json!({"rank_by": [["b", dir]], "top_k": 100})).await;
        assert_eq!(pair.len(), 6);
        assert_eq!(list, pair, "{dir}");
    }
}

#[tokio::test]
async fn a_list_that_cannot_mean_an_order_is_refused_before_any_blob_request() {
    let store = Accounted::new(MemoryStore::new());
    let api = loaded(store.clone()).await;
    let spent =
        || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(15), c));
    let nine: Vec<Value> = (0..9).map(|i| json!([format!("k{i}"), "asc"])).collect();
    let eight: Vec<Value> = (0..8).map(|i| json!([format!("k{i}"), "asc"])).collect();
    let two = json!([["a", "asc"], ["b", "desc"]]);
    for body in [
        json!({"rank_by": []}),
        json!({"rank_by": nine}),
        json!({"rank_by": [["a", "asc"], ["a", "desc"]]}),
        json!({"rank_by": [["id", "asc"], ["a", "asc"]]}),
        json!({"rank_by": [["a"]]}),
        json!({"rank_by": [["a", "up"]]}),
        json!({"rank_by": [["a", "asc"], "b"]}),
        json!({"rank_by": two, "vector": [1.0, 0.5]}),
        json!({"rank_by": two, "exact": true}),
        json!({"rank_by": two, "top_k": 9000, "offset": 1001}),
    ] {
        let before = spent();
        let (s, b) = query(&api, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
        assert_eq!(b["error"]["code"], "bad_request", "{body} -> {b}");
        assert_eq!(before, spent(), "{body}: the refusal made blob requests");
    }
    // The bounds themselves are allowed.
    for body in [
        json!({"rank_by": eight, "top_k": 10}),
        json!({"rank_by": two, "top_k": 9000, "offset": 1000}),
    ] {
        let (s, b) = query(&api, &body).await;
        assert_eq!(s, StatusCode::OK, "{body} -> {b}");
    }
}
