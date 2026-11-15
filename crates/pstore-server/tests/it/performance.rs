//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Per-query timings and the cache hit ratio (M264).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_cache::CacheCore;
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(r).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn query(api: &A, q: &Value) -> Value {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", q).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b
}

/// A folded index of 200 rows, through `api`.
async fn folded(api: &A) {
    let documents: Vec<Value> = (0..200_u32)
        .map(|i| {
            let x = f64::from(i);
            json!({"id": format!("d{i:03}"), "vector": [x.sin(), x.cos()],
                   "text": format!("row {i} word{}", i % 7), "attributes": {"n": i}})
        })
        .collect();
    let body = json!({"durability": "durable", "documents": documents,
                      "schema": {"text": {"type": "string", "full_text_search": true}}});
    let (s, b) = send(api, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn cached(store: &Accounted<MemoryStore>) -> A {
    Api::with_cache_unsecured(
        store.clone(),
        LaneId(1),
        Arc::new(CacheCore::memory(16 << 20)),
    )
    .unwrap()
}

fn q() -> Value {
    json!({"vector": [0.3, 0.9], "top_k": 5, "filters": ["n", "Gte", 10]})
}

#[tokio::test]
async fn performance_is_reported() {
    let store = Accounted::new(MemoryStore::new());
    let api = cached(&store);
    folded(&api).await;
    let single = query(&api, &q()).await;
    let multi = query(
        &api,
        &json!({"queries": [q(), {"text": "word3", "top_k": 3}]}),
    )
    .await;
    for got in [&single, &multi] {
        let p = &got["meta"]["performance"];
        let keys: Vec<&str> = p.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "cache_hit_bytes",
                "cache_hit_ratio",
                "cache_miss_bytes",
                "query_execution_ms",
                "server_total_ms"
            ],
            "{got}"
        );
        let total = p["server_total_ms"].as_f64().unwrap();
        let exec = p["query_execution_ms"].as_f64().unwrap();
        assert!(total.is_finite() && exec.is_finite(), "{p}");
        assert!(exec >= 0.0 && exec <= total, "{p}");
        assert!(
            exec > 0.0,
            "a query that read a segment took some time: {p}"
        );
    }
}

#[tokio::test]
async fn cache_ratio_cold_then_warm() {
    let store = Accounted::new(MemoryStore::new());
    let writer = cached(&store);
    folded(&writer).await;
    // A second server over the same store: its cache holds nothing yet.
    let api = cached(&store);
    let cold = query(&api, &q()).await;
    let p = &cold["meta"]["performance"];
    let (hit, miss) = (
        p["cache_hit_bytes"].as_u64().unwrap(),
        p["cache_miss_bytes"].as_u64().unwrap(),
    );
    assert!(miss > 0, "cold: {cold}");
    // What missed was read from the store, alongside uncached reads such as the HEAD.
    assert!(
        miss <= cold["meta"]["cost"]["bytes_read"].as_u64().unwrap(),
        "{cold}"
    );
    let ratio = p["cache_hit_ratio"].as_f64().unwrap();
    #[allow(clippy::cast_precision_loss, reason = "a test's ratio")]
    let want = hit as f64 / (hit + miss) as f64;
    assert_eq!(ratio, want, "{p}");
    assert!(ratio < 1.0, "{p}");
    let warm = query(&api, &q()).await;
    let w = &warm["meta"]["performance"];
    assert_eq!(w["cache_miss_bytes"], 0, "warm: {warm}");
    assert_eq!(w["cache_hit_bytes"], hit + miss, "warm: {warm}");
    assert_eq!(w["cache_hit_ratio"], 1.0, "warm: {warm}");
    assert_eq!(cold["results"], warm["results"]);
}

#[tokio::test]
async fn no_cache_reports_null_ratio() {
    let store = Accounted::new(MemoryStore::new());
    let api = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    folded(&api).await;
    for _ in 0..2 {
        let got = query(&api, &q()).await;
        let p = &got["meta"]["performance"];
        assert_eq!(p["cache_hit_ratio"], Value::Null, "{got}");
        assert_eq!(p["cache_hit_bytes"], 0, "{got}");
        assert_eq!(p["cache_miss_bytes"], 0, "{got}");
        assert!(
            got["meta"]["cost"]["bytes_read"].as_u64().unwrap() > 0,
            "{got}"
        );
    }
}
