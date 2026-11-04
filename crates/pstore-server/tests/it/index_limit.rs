//! M169: a tenant past `PSTORE_INDEXES_PER_TENANT` is refused a new index, `409 index_limit`.
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

type A = Arc<Api<MemoryStore>>;

async fn send(api: &A, tenant: u64, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("PUT")
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

async fn write(api: &A, tenant: u64, index: &str) -> (StatusCode, Value) {
    send(
        api,
        tenant,
        &format!("/v1/indexes/{index}/documents"),
        &json!({"durability": "durable", "documents": [{"id": "a", "vector": [1.0, 0.5]}]}),
    )
    .await
}

#[tokio::test]
async fn a_refusal_is_409() {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    api.limit_indexes(2);
    for n in ["a", "b"] {
        let (s, b) = write(&api, 7, n).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    let (s, b) = write(&api, 7, "c").await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "index_limit", "{b}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("2 indexes"),
        "{b}"
    );
    let (s, b) = send(&api, 7, "/v1/indexes/c", &json!({"dims": 2})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "index_limit", "{b}");
    // An index already held is written, and the limit is per tenant.
    let (s, b) = write(&api, 7, "a").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = write(&api, 8, "c").await;
    assert_eq!(s, StatusCode::OK, "{b}");
}
