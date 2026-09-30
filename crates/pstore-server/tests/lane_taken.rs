//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Two servers on one lane (M17): the one that finds its sequence taken says so, by name.

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

async fn put(api: &A, id: &str) -> (StatusCode, Value) {
    let body = json!({"durability": "durable",
                      "documents": [{"id": id, "vector": [1.0, 0.5]}]});
    let req = Request::builder()
        .method("PUT")
        .uri("/v1/indexes/idx/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "71")
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

#[tokio::test]
async fn a_write_on_a_taken_lane_answers_lane_taken() {
    let store = Accounted::new(MemoryStore::new());
    let a = Api::new(store.clone(), LaneId(4)).unwrap();
    let b = Api::new(store, LaneId(4)).unwrap();
    for (api, id) in [(&a, "a1"), (&b, "b1")] {
        let (s, body) = put(api, id).await;
        assert_eq!(s, StatusCode::OK, "{body}");
    }
    let (s, body) = put(&a, "a2").await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "lane_taken", "{body}");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("lane 4") && msg.contains("PSTORE_LANE"),
        "{msg}"
    );
}
