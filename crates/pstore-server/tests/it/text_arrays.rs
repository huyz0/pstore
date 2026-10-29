//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M150: a text field holding an array of strings is indexed as its strings, and scores as the
//! same strings joined by a space would.

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

fn api() -> A {
    Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap()
}

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "52")
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

/// Writes `(id, text)` rows into `index`, the text under `attributes`.
async fn put(api: &A, index: &str, rows: &[(&str, Value)]) {
    let docs: Vec<Value> = rows
        .iter()
        .map(|(id, t)| json!({"id": id, "vector": [1.0, 0.5], "attributes": {"text": t}}))
        .collect();
    let body = json!({"durability": "durable", "documents": docs});
    let (s, b) = send(api, "PUT", &format!("/v1/indexes/{index}/documents"), &body).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// A `text` query's results: each id with its score.
async fn ranked(api: &A, index: &str, q: &str) -> Vec<(String, Value)> {
    let body = json!({"text": q, "top_k": 100});
    let (s, b) = send(api, "POST", &format!("/v1/indexes/{index}/query"), &body).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].as_str().unwrap().to_owned(), r["score"].clone()))
        .collect()
}

/// Each array's strings joined by a space; a string as it is.
fn joined(v: &Value) -> Value {
    match v {
        Value::Array(items) => Value::String(
            items
                .iter()
                .map(|e| e.as_str().unwrap())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        v => v.clone(),
    }
}

const QUERIES: [&str; 6] = [
    "fox",
    "quick fox",
    "the dog",
    "lazy",
    "brown quick lazy",
    "zebra",
];

async fn twins_agree(rows: &[(&str, Value)]) {
    let a = api();
    let flat: Vec<(&str, Value)> = rows.iter().map(|(id, v)| (*id, joined(v))).collect();
    put(&a, "arr", rows).await;
    put(&a, "flat", &flat).await;
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        let mut hits = 0;
        for q in QUERIES {
            let (x, y) = (ranked(&a, "arr", q).await, ranked(&a, "flat", q).await);
            assert_eq!(x, y, "{q:?} folded: {folded}");
            hits += x.len();
        }
        assert!(hits > 0, "no query found anything, folded: {folded}");
    }
}

#[tokio::test]
async fn an_array_text_field_scores_as_its_strings_joined() {
    // M150 criterion 2: arrays only, so no string row can index the field for them; then a mix.
    let arrays = [
        ("a", json!(["the quick", "brown fox"])),
        ("b", json!(["lazy dog", "", "dog"])),
        ("c", json!(["fox fox", "fox"])),
        ("d", json!(["quick brown lazy fox jumps"])),
    ];
    twins_agree(&arrays).await;
    let mut mixed = arrays.to_vec();
    mixed.push(("e", json!("the quick dog")));
    mixed.push(("f", json!("fox")));
    twins_agree(&mixed).await;
}

#[tokio::test]
async fn an_array_with_no_text_is_no_text() {
    // M150 criterion 3.
    let a = api();
    put(
        &a,
        "docs",
        &[
            ("a", json!(["lazy dog", "fox"])),
            ("e1", json!([])),
            ("e2", json!([""])),
            ("f", json!(["", "fox"])),
        ],
    )
    .await;
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        let ids = |r: Vec<(String, Value)>| r.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        let mut found = ids(ranked(&a, "docs", "fox").await);
        found.sort();
        assert_eq!(found, ["a", "f"], "folded: {folded}");
        for q in ["lazy dog fox", "the", "a"] {
            let got = ids(ranked(&a, "docs", q).await);
            assert!(
                !got.iter().any(|id| id.starts_with('e')),
                "{q:?} found {got:?}, folded: {folded}"
            );
        }
    }
}

#[tokio::test]
async fn the_door_takes_an_array_of_strings_and_nothing_else_as_text() {
    // M150 criterion 4.
    let a = api();
    let doc = |t: Value| json!({"id": "x", "vector": [1.0, 0.5], "attributes": {"text": t}});
    for (body, why) in [
        (json!({"documents": [doc(json!(["a", 5]))]}), "text"),
        (json!({"documents": [doc(json!([["a"]]))]}), "text"),
        (
            json!({"schema": {"text": {"type": "int", "full_text_search": true}},
                   "documents": [doc(json!("a"))]}),
            "the text field's type",
        ),
        (
            json!({"schema": {"text": {"type": "[]int", "full_text_search": true}},
                   "documents": [doc(json!(["a"]))]}),
            "the text field's type",
        ),
    ] {
        let (s, b) = send(&a, "PUT", "/v1/indexes/docs/documents", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} was stored: {b}");
        let msg = b["error"]["message"].as_str().unwrap();
        assert!(msg.contains(why), "{body}: {msg:?} does not name {why:?}");
    }
    let body = json!({"schema": {"text": {"type": "[]string", "full_text_search": true}},
                      "durability": "durable", "documents": [doc(json!(["fox", "dog"]))]});
    let (s, b) = send(&a, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ranked(&a, "docs", "dog").await.len(), 1);
}

#[tokio::test]
async fn an_array_text_field_is_not_highlighted() {
    // M150 criterion 6, a guard: structured highlights over arrays are a later milestone.
    let a = api();
    put(&a, "docs", &[("a", json!(["quick fox", "dog"]))]).await;
    let q = json!({"text": "fox", "top_k": 10, "highlight": {}});
    let (s, b) = send(&a, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let r = b["results"].as_array().unwrap();
    assert_eq!(r.len(), 1, "{b}");
    assert!(r[0].get("$highlight").is_none_or(Value::is_null), "{b}");
}
