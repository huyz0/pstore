//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A rejected row's quarantine over HTTP (M25). Two writers create one index under different
//! analyzers, as `analyzer.rs` does, so one fold rejects one of their rows.

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

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "45")
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

async fn put(api: &A, id: &str, text: &str, fts: Value) {
    let body = json!({
        "durability": "durable",
        "documents": [{"id": id, "vector": [1.0, 0.5], "text": text, "attributes": {"n": 7}}],
        "schema": {"text": {"type": "string", "full_text_search": fts}},
    });
    let (s, b) = send(api, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Two processes, each creating `docs` under its own analyzer; one fold rejects one row.
async fn rejected() -> (A, A) {
    let store = Accounted::new(MemoryStore::new());
    let a = Api::new(store.clone(), LaneId(1)).unwrap();
    let b = Api::new(store, LaneId(2)).unwrap();
    put(&a, "r", "he runs daily", json!({"stemming": true})).await;
    put(&b, "s", "she runs daily", json!({"case_sensitive": true})).await;
    let (s, body) = send(&a, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    (a, b)
}

#[tokio::test]
async fn quarantine_is_exported_counted_and_discarded() {
    let (a, _) = rejected().await;
    let (s, meta) = send(&a, "GET", "/v1/indexes/docs", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{meta}");
    assert_eq!(meta["rejected_rows"], 1, "{meta}");
    assert_eq!(meta["quarantined_rows"], 1, "{meta}");

    let (s, q) = send(&a, "GET", "/v1/indexes/docs/quarantine", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{q}");
    let rows = q["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{q}");
    let row = &rows[0];
    let id = row["document"]["id"].as_str().unwrap();
    assert!(id == "r" || id == "s", "{q}");
    assert_eq!(row["document"]["vector"], json!([1.0, 0.5]), "{q}");
    assert_eq!(row["document"]["attributes"]["n"], 7, "{q}");
    // The declared analyzer is why it was rejected, and the door refuses it on a write.
    assert!(row["reserved"]["$fts"].is_string(), "{q}");
    assert!(
        row["document"]["attributes"]
            .as_object()
            .unwrap()
            .keys()
            .all(|k| !k.starts_with('$')),
        "{q}"
    );
    assert!(row["reason"].is_string(), "{q}");

    let through = q["epoch"].as_u64().unwrap();
    let (s, d) = send(
        &a,
        "DELETE",
        &format!("/v1/indexes/docs/quarantine?through={through}"),
        &Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["discarded"], 1, "{d}");
    let (_, q) = send(&a, "GET", "/v1/indexes/docs/quarantine", &Value::Null).await;
    assert_eq!(q["rows"], json!([]), "{q}");
    let (_, meta) = send(&a, "GET", "/v1/indexes/docs", &Value::Null).await;
    assert_eq!(meta["quarantined_rows"], 0, "{meta}");
    assert_eq!(
        meta["rejected_rows"], 1,
        "the count of rows ever rejected moved: {meta}"
    );
    // Nothing left: no commit.
    let (s, d) = send(
        &a,
        "DELETE",
        &format!("/v1/indexes/docs/quarantine?through={through}"),
        &Value::Null,
    )
    .await;
    assert_eq!(
        (s, d["discarded"].clone()),
        (StatusCode::OK, json!(0)),
        "{d}"
    );
}

#[tokio::test]
async fn a_discard_without_a_through_is_refused() {
    let (a, _) = rejected().await;
    for uri in [
        "/v1/indexes/docs/quarantine",
        "/v1/indexes/docs/quarantine?through=soon",
    ] {
        let (s, b) = send(&a, "DELETE", uri, &Value::Null).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{uri}: {b}");
    }
    let (_, meta) = send(&a, "GET", "/v1/indexes/docs", &Value::Null).await;
    assert_eq!(
        meta["quarantined_rows"], 1,
        "a refused discard discarded: {meta}"
    );
}

#[tokio::test]
async fn quarantine_of_an_unknown_index_is_404() {
    let (a, _) = rejected().await;
    let (s, b) = send(&a, "GET", "/v1/indexes/nope/quarantine", &Value::Null).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "index_not_found", "{b}");
    let (s, b) = send(
        &a,
        "DELETE",
        "/v1/indexes/nope/quarantine?through=9",
        &Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}
