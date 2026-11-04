//! M149: `PATCH /v1/indexes/{index}` makes an index read-only, and every write route
//! refuses it.
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
use pstore_server::{Api, FoldPolicy};
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const TENANT: u64 = 149;

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

fn flag(on: bool) -> Value {
    json!({ "read_only": on })
}

fn tenant() -> TenantId {
    TenantId(u128::from(TENANT))
}

async fn write(api: &A, body: &Value) -> (StatusCode, Value) {
    send(api, "PUT", "/v1/indexes/t/documents", body).await
}

async fn summary(api: &A) -> Value {
    let (s, b) = send(api, "GET", "/v1/indexes/t", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

#[tokio::test]
async fn read_only_survives_restart_and_refuses_writes() {
    let store = Accounted::new(MemoryStore::new());
    let api = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    let (s, b) = write(
        &api,
        &json!({"durability": "durable", "documents": [
            {"id": "a", "vector": [1.0, 0.5], "attributes": {"n": 1}}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&api).await;
    assert_eq!(summary(&api).await["read_only"], false);
    let (reads, puts) = (
        store.count(tenant(), OpClass::Read),
        store.count(tenant(), OpClass::Write),
    );
    let (s, b) = send(&api, "PATCH", "/v1/indexes/t", &flag(true)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["read_only"], true, "{b}");
    assert_eq!(
        (
            b["cost"]["blob_reads"].clone(),
            b["cost"]["blob_writes"].clone()
        ),
        (json!(1), json!(1)),
        "{b}"
    );
    assert_eq!(store.count(tenant(), OpClass::Read) - reads, 1);
    assert_eq!(store.count(tenant(), OpClass::Write) - puts, 1);
    let set = b["epoch"].clone();
    // The value it has: a read, no write, the same epoch.
    let (s, b) = send(&api, "PATCH", "/v1/indexes/t", &flag(true)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["epoch"], set, "{b}");
    assert_eq!(b["cost"]["blob_writes"], 0, "{b}");
    // From this process and from a second one over the same store.
    let other = Api::unsecured(store.clone(), LaneId(2)).unwrap();
    for api in [&api, &other] {
        assert_eq!(summary(api).await["read_only"], true);
        let puts = store.count(tenant(), OpClass::Write);
        for body in [
            json!({"durability": "durable", "documents": [{"id": "x", "vector": [1.0, 0.5]}]}),
            json!({"durability": "durable", "patch_rows": [{"id": "a", "attributes": {"n": 2}}]}),
            json!({"durability": "durable", "deletes": ["a"]}),
            json!({"durability": "durable", "documents": [{"id": "a", "vector": [1.0, 0.5]}],
                   "upsert_condition": ["n", "Eq", 1]}),
            json!({"durability": "durable", "delete_by_filter": ["n", "Eq", 1]}),
            json!({"durability": "durable", "patch_by_filter": {"filters": ["n", "Eq", 1], "attributes": {"m": 9}}}),
        ] {
            let (s, b) = write(api, &body).await;
            assert_eq!(s, StatusCode::CONFLICT, "{body}: {b}");
            assert_eq!(b["error"]["code"], "index_read_only", "{body}: {b}");
        }
        let (s, b) = send(api, "DELETE", "/v1/indexes/t", &Value::Null).await;
        assert_eq!(
            (s, b["error"]["code"].clone()),
            (StatusCode::CONFLICT, json!("index_read_only")),
            "{b}"
        );
        fold(api).await;
        assert_eq!(
            store.count(tenant(), OpClass::Write),
            puts,
            "a refused write wrote"
        );
    }
    // Cleared, and a third process sees it cleared and writes.
    let (s, b) = send(&other, "PATCH", "/v1/indexes/t", &flag(false)).await;
    assert_eq!(
        (s, b["read_only"].clone()),
        (StatusCode::OK, json!(false)),
        "{b}"
    );
    let third = Api::unsecured(store.clone(), LaneId(3)).unwrap();
    assert_eq!(summary(&third).await["read_only"], false);
    let (s, b) = write(
        &third,
        &json!({"durability": "durable", "documents": [{"id": "b", "vector": [1.0, 0.5]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&third).await;
    assert_eq!(summary(&third).await["documents"], 2);
}

#[tokio::test]
async fn patch_refusals() {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    let (s, b) = send(&api, "PATCH", "/v1/indexes/t", &flag(true)).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::NOT_FOUND, json!("index_not_found")),
        "{b}"
    );
    // Held unfolded by this process: its summary reports it, and it cannot be flagged yet.
    let (s, b) = write(
        &api,
        &json!({"documents": [{"id": "a", "vector": [1.0, 0.5]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(summary(&api).await["unfolded"], true);
    let (s, b) = send(&api, "PATCH", "/v1/indexes/t", &flag(true)).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("index_not_folded")),
        "{b}"
    );
    for body in [
        json!({}),
        json!({"read_only": "yes"}),
        json!({"read_only": true, "pinned": true}),
    ] {
        let (s, b) = send(&api, "PATCH", "/v1/indexes/t", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}: {b}");
    }
}
