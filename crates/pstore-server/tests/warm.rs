//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `POST /v1/indexes/{index}/warm` (M21): billed, refused by name where it cannot help.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_cache::CacheCore;
use pstore_server::{Api, FoldPolicy};
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const TENANT: u64 = 37;

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT.to_string())
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

async fn write(api: &A, index: &str, ids: std::ops::Range<u32>) {
    let docs: Vec<Value> = ids
        .map(|i| json!({"id": format!("d{i}"), "vector": [f64::from(i).sin(), 1.0]}))
        .collect();
    let (s, b) = send(
        api,
        "PUT",
        &format!("/v1/indexes/{index}/documents"),
        &json!({"durability": "durable", "documents": docs}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold(api: &A) {
    api.fold_due(&FoldPolicy {
        period: std::time::Duration::from_secs(1),
        age: std::time::Duration::ZERO,
        bytes: 1,
    })
    .await;
}

fn reads(acct: &Accounted<MemoryStore>) -> u64 {
    acct.count(TenantId(u128::from(TENANT)), OpClass::Read)
}

/// Two folded segments of `docs`, and a cached `Api` over them whose cache is still cold:
/// the folds' own reads warmed the cache of the process that made them.
async fn cached() -> (A, Accounted<MemoryStore>) {
    let acct = Accounted::new(MemoryStore::new());
    let writer = Api::new(acct.clone(), LaneId(1)).unwrap();
    write(&writer, "docs", 0..20).await;
    fold(&writer).await;
    write(&writer, "docs", 20..40).await;
    fold(&writer).await;
    let core = Arc::new(CacheCore::memory(64 << 20));
    (
        Api::with_cache(acct.clone(), LaneId(2), core).unwrap(),
        acct,
    )
}

#[tokio::test]
async fn warm_reports_and_bills() {
    let (api, acct) = cached().await;
    let before = reads(&acct);
    let (s, b) = send(&api, "POST", "/v1/indexes/docs/warm", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let spent = reads(&acct) - before;
    // HEAD and two footers: nothing else exists below the clustering threshold.
    assert_eq!(spent, 3);
    assert_eq!(b["segments"], 2, "{b}");
    assert_eq!(b["fetched"], 0, "{b}");
    assert_eq!(b["meta"]["cost"]["blob_reads"], spent, "{b}");
    // Warm now: HEAD alone, and billed as such.
    let (_, b) = send(&api, "POST", "/v1/indexes/docs/warm", &Value::Null).await;
    assert_eq!(b["meta"]["cost"]["blob_reads"], 1, "{b}");
}

#[tokio::test]
async fn warm_of_an_unfolded_index_is_empty() {
    let (api, _) = cached().await;
    write(&api, "fresh", 0..3).await;
    let (s, b) = send(&api, "POST", "/v1/indexes/fresh/warm", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["segments"].as_u64(), b["fetched"].as_u64()),
        (Some(0), Some(0))
    );
}

#[tokio::test]
async fn warm_of_a_missing_index_is_404() {
    let (api, _) = cached().await;
    let (s, b) = send(&api, "POST", "/v1/indexes/nope/warm", &Value::Null).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "index_not_found", "{b}");
}

#[tokio::test]
async fn warm_without_a_cache_is_409_and_free() {
    let acct = Accounted::new(MemoryStore::new());
    let api = Api::new(acct.clone(), LaneId(1)).unwrap();
    write(&api, "docs", 0..5).await;
    fold(&api).await;
    let before = reads(&acct);
    let (s, b) = send(&api, "POST", "/v1/indexes/docs/warm", &Value::Null).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "no_read_cache", "{b}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("PSTORE_CACHE_DIR"),
        "{b}"
    );
    assert_eq!(reads(&acct), before, "a refused warm read the store");
}

#[tokio::test]
async fn a_warm_is_billed_to_its_own_tenant() {
    let (api, acct) = cached().await;
    let other = TenantId(u128::from(TENANT) + 1);
    let before = acct.count(other, OpClass::Read);
    send(&api, "POST", "/v1/indexes/docs/warm", &Value::Null).await;
    assert_eq!(acct.count(other, OpClass::Read), before);
}
