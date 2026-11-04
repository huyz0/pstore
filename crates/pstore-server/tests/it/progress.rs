//! M160: `GET /v1/indexes/{index}?progress=true` reports the operations every writer has
//! made durable and no fold has indexed.
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
use pstore_server::{Api, FoldPolicy};
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const TENANT: u64 = 160;

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

async fn fold(api: &A) {
    api.fold_due(&FoldPolicy {
        period: std::time::Duration::from_secs(1),
        age: std::time::Duration::ZERO,
        bytes: 1,
    })
    .await;
}

async fn durable(api: &A, index: &str, body: Value) {
    let mut body = body;
    body["durability"] = json!("durable");
    let (s, b) = send(api, "PUT", &format!("/v1/indexes/{index}/documents"), &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn get(api: &A, uri: &str) -> (StatusCode, Value) {
    send(api, "GET", uri, &Value::Null).await
}

#[tokio::test]
async fn summary_progress() {
    let store = Accounted::new(MemoryStore::new());
    let a = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    let b = Api::unsecured(store, LaneId(2)).unwrap();
    durable(
        &a,
        "t",
        json!({"documents": [{"id": "a", "vector": [1.0, 0.5]}]}),
    )
    .await;
    fold(&a).await;
    // Without the parameter, or with it false: no progress, one read.
    for uri in ["/v1/indexes/t", "/v1/indexes/t?progress=false"] {
        let (s, got) = get(&a, uri).await;
        assert_eq!(s, StatusCode::OK, "{got}");
        assert!(got.get("progress").is_none(), "{got}");
        assert_eq!(got["cost"]["blob_reads"], 1, "{got}");
    }
    let (_, got) = get(&a, "/v1/indexes/t?progress=true").await;
    assert_eq!(
        got["progress"],
        json!({"status": "up-to-date", "unindexed_rows": 0, "unindexed_bytes": 0,
               "exact": true, "as_of_epoch": got["epoch"]}),
        "{got}"
    );
    // Another process's durable operations, on a held index and on a new one.
    durable(
        &b,
        "t",
        json!({"documents": [{"id": "x", "vector": [1.0, 0.5]},
                                          {"id": "y", "vector": [1.0, 0.5]}],
                            "deletes": ["a"]}),
    )
    .await;
    durable(
        &b,
        "fresh",
        json!({"documents": [{"id": "f", "vector": [1.0, 0.5]}]}),
    )
    .await;
    let (s, got) = get(&a, "/v1/indexes/t?progress=true").await;
    assert_eq!(s, StatusCode::OK, "{got}");
    let p = &got["progress"];
    assert_eq!(p["status"], "updating", "{got}");
    assert_eq!(p["unindexed_rows"], 3, "{got}");
    assert!(p["unindexed_bytes"].as_u64().unwrap() > 0, "{got}");
    assert_eq!(p["exact"], true, "{got}");
    assert_eq!(got["documents"], 1, "the folded count is unchanged: {got}");
    // `a` holds nothing of `fresh`, and HEAD does not: only its progress finds it.
    let (s, got) = get(&a, "/v1/indexes/fresh").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{got}");
    let (s, got) = get(&a, "/v1/indexes/fresh?progress=true").await;
    assert_eq!(s, StatusCode::OK, "{got}");
    assert_eq!(
        (got["segments"].clone(), got["documents"].clone()),
        (json!(0), json!(0))
    );
    assert_eq!(got["progress"]["unindexed_rows"], 1, "{got}");
    // Folded, by the writer that holds them: up to date, as of that fold.
    fold(&b).await;
    let (_, got) = get(&a, "/v1/indexes/t?progress=true").await;
    assert_eq!(got["progress"]["status"], "up-to-date", "{got}");
    assert_eq!(got["progress"]["as_of_epoch"], got["epoch"], "{got}");
    assert_eq!(got["documents"], 2, "{got}");
    // Refusals.
    let (s, got) = get(&a, "/v1/indexes/t?progress=maybe").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{got}");
    for uri in ["/v1/indexes/absent", "/v1/indexes/absent?progress=true"] {
        let (s, got) = get(&a, uri).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{got}");
    }
}
