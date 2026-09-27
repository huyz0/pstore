//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A server restarted on its stable lane loses nothing it acknowledged (M9j, BACKLOG row 39).

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
        .header("x-pstore-tenant", "19")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn put(api: &Arc<Api<MemoryStore>>, id: &str) {
    let body = json!({"durability": "durable", "documents": [
        {"id": id, "vector": [1.0, 0.5]}]});
    let (s, b) = send(
        api,
        request("PUT", "/v1/indexes/docs/documents", Some(&body)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold(api: &Arc<Api<MemoryStore>>) {
    let (s, b) = send(api, request("POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn a_restart_on_the_same_lane_keeps_every_acknowledged_write() {
    let store = Accounted::new(MemoryStore::new());
    let before = Api::new(store.clone(), LaneId(1)).unwrap();
    put(&before, "a").await;
    fold(&before).await;
    put(&before, "b").await;
    drop(before);

    let after = Api::new(store, LaneId(1)).unwrap();
    put(&after, "c").await;
    fold(&after).await;
    let q = json!({"vector": [1.0, 0.5], "top_k": 10});
    let (s, b) = send(&after, request("POST", "/v1/indexes/docs/query", Some(&q))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut ids: Vec<&str> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["a", "b", "c"]);
}
