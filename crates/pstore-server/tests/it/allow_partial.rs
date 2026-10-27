//! `allow_partial` by-filter writes over HTTP (M137): applied up to the bound, answered with
//! `rows_remaining`, resumed by sending the write again.
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
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

/// A server at `PSTORE_APPLIED_WRITES=level`, whose by-filter operations are bounded at 3.
fn api(level: u8) -> Arc<Api<MemoryStore>> {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    api.set_applied_writes(level >= 1);
    api.set_affected_ids(level >= 2);
    api.set_idempotency(level >= 3);
    api.set_partial_filters(level >= 4);
    api.set_filter_rows_for_test(3);
    api
}

async fn write(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "9")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

/// Five rows with `n = 1`, folded by an applied write.
async fn seeded(api: &Arc<Api<MemoryStore>>) {
    let docs: Vec<Value> = (0..5)
        .map(|i| json!({"id": format!("r{i}"), "vector": [1.0, 0.5], "attributes": {"n": 1}}))
        .collect();
    let (s, b) = write(api, &json!({"durability": "applied", "documents": docs})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn partial_delete() -> Value {
    json!({"durability": "applied", "delete_by_filter": ["n", "Eq", 1],
           "delete_by_filter_allow_partial": true})
}

#[tokio::test]
async fn a_partial_delete_says_what_remains() {
    let a = api(4);
    seeded(&a).await;
    let (s, b) = write(&a, &partial_delete()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["applied"]["deleted"], 3, "{b}");
    assert_eq!(b["applied"]["rows_remaining"], true, "{b}");
    let (s, b) = write(&a, &partial_delete()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["applied"]["deleted"], 2, "{b}");
    assert!(b["applied"].get("rows_remaining").is_none(), "{b}");
}

#[tokio::test]
async fn a_partial_patch_says_what_remains() {
    let a = api(4);
    seeded(&a).await;
    let body = json!({"durability": "applied",
        "patch_by_filter": {"filters": ["n", "Eq", 1], "attributes": {"m": 9}},
        "patch_by_filter_allow_partial": true});
    let (s, b) = write(&a, &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["applied"]["patched"], 3, "{b}");
    assert_eq!(b["applied"]["rows_remaining"], true, "{b}");
    let (s, b) = write(&a, &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["applied"]["patched"], 2, "{b}");
    assert!(b["applied"].get("rows_remaining").is_none(), "{b}");
    // A plain one past the bound is still refused whole (M136).
    let plain = json!({"durability": "applied",
        "patch_by_filter": {"filters": ["n", "Eq", 1], "attributes": {"m": 10}}});
    let (s, b) = write(&a, &plain).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["applied"]["rejected"], 1, "{b}");
    assert!(b["applied"].get("rows_remaining").is_none(), "{b}");
}

#[tokio::test]
async fn allow_partial_refusals() {
    let a = api(4);
    let refused = |b: &Value, code: &str| b["error"]["code"] == code;
    // Without an applied answer, nobody could be told what remains.
    let mut body = partial_delete();
    body["durability"] = json!("durable");
    let (s, b) = write(&a, &body).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(refused(&b, "bad_request"), "{b}");
    // Without its by-filter operation.
    for flag in [
        "delete_by_filter_allow_partial",
        "patch_by_filter_allow_partial",
    ] {
        let body = json!({"durability": "applied",
            "documents": [{"id": "x", "vector": [1.0, 0.5]}], flag: true});
        let (s, b) = write(&a, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{flag}: {b}");
        assert!(refused(&b, "bad_request"), "{flag}: {b}");
    }
    // Below level 4.
    let low = api(3);
    let (s, b) = write(&low, &partial_delete()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(refused(&b, "unsupported_durability"), "{b}");
}
