//! Idempotency keys over HTTP (M132): an applied write carrying a key the server issued
//! applies at most once; a retry is answered with the winner's outcome, and one the fold
//! cannot prove is not a repeat is refused.
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
use pstore_engine::OUTCOMES_KEPT;
use pstore_server::Api;
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

const TENANT: TenantId = TenantId(9);

/// A server at `PSTORE_APPLIED_WRITES=level`.
fn api(store: &Accounted<MemoryStore>, level: u8) -> Arc<Api<MemoryStore>> {
    let api = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    api.set_applied_writes(level >= 1);
    api.set_affected_ids(level >= 2);
    api.set_idempotency(level >= 3);
    api
}

async fn send(
    api: &Arc<Api<MemoryStore>>,
    method: &str,
    path: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "9")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn issue(api: &Arc<Api<MemoryStore>>) -> (StatusCode, Value) {
    send(api, "POST", "/v1/indexes/docs/idempotency-keys", &json!({})).await
}

async fn key(api: &Arc<Api<MemoryStore>>) -> String {
    let (s, b) = issue(api).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["idempotency_key"].as_str().unwrap().to_owned()
}

fn doc(id: &str, n: i64) -> Value {
    json!({"id": id, "vector": [1.0, 0.5], "attributes": {"n": n}})
}

async fn write(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(api, "PUT", "/v1/indexes/docs/documents", body).await
}

fn keyed(key: &str, id: &str, n: i64) -> Value {
    json!({"durability": "applied", "idempotency_key": key, "documents": [doc(id, n)]})
}

/// Every row of `docs`, as `id -> n`.
async fn rows(api: &Arc<Api<MemoryStore>>) -> std::collections::BTreeMap<String, i64> {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 100, "consistency": "strong",
                   "include_attributes": true});
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                r["attributes"]["n"].as_i64().unwrap(),
            )
        })
        .collect()
}

/// Criterion 1 over HTTP: the retry answers the first attempt's counts, `replayed`, and
/// applies nothing.
#[tokio::test]
async fn a_retry_is_answered_not_applied() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 3);
    let k = key(&a).await;
    let (s, first) = write(&a, &keyed(&k, "x", 1)).await;
    assert_eq!(s, StatusCode::OK, "{first}");
    assert_eq!(first["applied"]["upserted"], 1, "{first}");
    assert!(first.get("replayed").is_none(), "{first}");
    let (s, b) = write(
        &a,
        &json!({"durability": "applied", "documents": [doc("x", 2)]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, retry) = write(&a, &keyed(&k, "x", 1)).await;
    assert_eq!(s, StatusCode::OK, "{retry}");
    assert_eq!(retry["replayed"], true, "{retry}");
    assert_eq!(retry["applied"]["upserted"], 1, "{retry}");
    assert_ne!(retry["operation"], first["operation"], "{retry}");
    assert_eq!(rows(&a).await["x"], 2);
}

/// Criterion 5 over HTTP: once no listed record names the winner, a retry is refused and
/// applies nothing.
#[tokio::test]
async fn an_expired_retry_is_refused() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 3);
    let k = key(&a).await;
    let (s, b) = write(&a, &keyed(&k, "x", 1)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for i in 0..OUTCOMES_KEPT {
        let body = json!({"durability": "applied", "documents": [doc(&format!("t{i}"), 1)]});
        let (s, b) = write(&a, &body).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    let (s, b) = write(&a, &keyed(&k, "late", 1)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "idempotency_expired", "{b}");
    assert!(!rows(&a).await.contains_key("late"));
}

/// Criterion 8: what is refused, and at which level.
#[tokio::test]
async fn refusals_and_levels() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 3);
    let k = key(&a).await;
    let refused = |b: &Value, code: &str| assert_eq!(b["error"]["code"], code, "{b}");
    for durability in ["batched", "durable"] {
        let body = json!({"durability": durability, "idempotency_key": k,
                          "documents": [doc("x", 1)]});
        let (s, b) = write(&a, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        refused(&b, "bad_request");
    }
    for bad in ["", "7", "7-xyz", "007-00000000000000000000000000000000"] {
        let (s, b) = write(&a, &keyed(bad, "x", 1)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {b}");
        refused(&b, "bad_request");
    }
    // Ahead of HEAD, `u64::MAX` included: refused before anything is buffered.
    for e0 in ["5", "18446744073709551615"] {
        let ahead = format!("{e0}-{:032x}", 1);
        let (s, b) = write(&a, &keyed(&ahead, "x", 1)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        refused(&b, "bad_request");
    }
    // Nothing was buffered: the index was never made.
    let q = json!({"rank_by": ["id", "asc"], "top_k": 10, "consistency": "strong"});
    let (s, b) = send(&a, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    // Below level 3: the field and the route are both refused.
    let two = api(&store, 2);
    let (s, b) = write(&two, &keyed(&k, "x", 1)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    refused(&b, "unsupported_durability");
    let (s, b) = issue(&two).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    refused(&b, "unsupported_durability");
}

/// Criterion 8: a key is issued at the engine's mark, at most HEAD's epoch, and while the
/// mark is fresh issuing one costs no blob request.
#[tokio::test]
async fn an_issued_key_is_free_while_the_mark_is_fresh() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 3);
    let (s, b) = write(
        &a,
        &json!({"durability": "applied", "documents": [doc("x", 1)]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let epoch = b["epoch"].as_u64().unwrap();
    let spent = || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TENANT, c));
    let before = spent();
    let k = key(&a).await;
    assert_eq!(spent(), before, "issuing a key made a blob request");
    let (e0, hex) = k.split_once('-').unwrap();
    assert_eq!(e0.parse::<u64>().unwrap(), epoch);
    assert_eq!(hex.len(), 32);
    assert_ne!(key(&a).await, k, "two keys alike");
}

/// §9.4: a refused attempt whose window fell off the list cannot tell whether part of it
/// applied earlier, and answers `idempotency_partial` rather than a clean refusal. Here the
/// window fell because another server committed 32 times inside this one's fresh mark.
#[tokio::test]
async fn a_refusal_whose_window_fell_is_partial() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store, 3);
    let b = Api::unsecured(store.clone(), LaneId(2)).unwrap();
    b.set_applied_writes(true);
    b.set_affected_ids(true);
    b.set_idempotency(true);
    let k = key(&a).await;
    let (s, body) = write(&a, &keyed(&k, "x", 1)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    for i in 0..OUTCOMES_KEPT {
        let body = json!({"durability": "applied", "documents": [doc(&format!("t{i}"), 1)]});
        let (s, body) = write(&b, &body).await;
        assert_eq!(s, StatusCode::OK, "{body}");
    }
    let (s, body) = write(&a, &keyed(&k, "late", 1)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "idempotency_partial", "{body}");
    assert!(!rows(&a).await.contains_key("late"));
}
