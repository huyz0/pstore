//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M181: the `word_v0` to `word_v4` tokenizers and `max_token_length` over HTTP.

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
        .header("x-pstore-tenant", "181")
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

const EMOJI: &str = "\u{1F469}\u{200D}\u{1F4BB}";

fn rows() -> Vec<(&'static str, String)> {
    vec![
        ("tower", "東京タワー".to_owned()),
        ("emoji", format!("{EMOJI} dev")),
        ("short", "abcde x".to_owned()),
        ("long", "abcdefgh y".to_owned()),
    ]
}

async fn create(api: &A, index: &str, fts: &Value) -> (StatusCode, Value) {
    let docs: Vec<Value> = rows()
        .into_iter()
        .enumerate()
        .map(|(i, (id, text))| {
            json!({"id": id, "vector": [1.0, i as f32], "attributes": {"text": text}})
        })
        .collect();
    let body = json!({
        "durability": "durable",
        "schema": {"text": {"type": "string", "full_text_search": fts}},
        "documents": docs,
    });
    send(api, "PUT", &format!("/v1/indexes/{index}/documents"), &body).await
}

/// A text query's ids, each with its score.
async fn ranked(api: &A, index: &str, q: &str) -> Vec<(String, f64)> {
    let body = json!({
        "text": [{"field": "text", "query": q}],
        "fusion": {"sum": {"weights": [1.0]}},
        "top_k": 10,
    });
    let (s, b) = send(api, "POST", &format!("/v1/indexes/{index}/query"), &body).await;
    assert_eq!(s, StatusCode::OK, "{index} {body} -> {b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                r["score"].as_f64().unwrap(),
            )
        })
        .collect()
}

async fn ids(api: &A, index: &str, q: &str) -> Vec<String> {
    ranked(api, index, q)
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

#[tokio::test]
async fn tokenizers_over_http() {
    let a = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    for (index, fts) in [
        ("v1", json!({"tokenizer": "word_v1"})),
        ("v2", json!({"tokenizer": "word_v2"})),
        ("v3", json!({"tokenizer": "word_v3"})),
        (
            "lim",
            json!({"tokenizer": "word_v3", "max_token_length": 5}),
        ),
    ] {
        let (s, b) = create(&a, index, &fts).await;
        assert_eq!(s, StatusCode::OK, "{index}: {b}");
    }
    for folded in [false, true] {
        if folded {
            let (s, b) = send(&a, "POST", "/v1/admin/fold", &json!({})).await;
            assert_eq!(s, StatusCode::OK, "{b}");
        }
        let tower = ranked(&a, "v3", "東京").await;
        assert_eq!(tower.len(), 1, "{tower:?}");
        assert_eq!(tower[0].0, "tower");
        assert!(tower[0].1 > 0.0, "{tower:?}");
        assert!(ids(&a, "v1", "東京").await.is_empty(), "folded {folded}");
        assert_eq!(ids(&a, "v1", "東京タワー").await, ["tower"]);
        assert_eq!(ids(&a, "v2", "京").await, ["tower"]);
        assert_eq!(ids(&a, "v2", EMOJI).await, ["emoji"]);
        assert!(ids(&a, "v1", EMOJI).await.is_empty(), "folded {folded}");
        // The long token is in neither the document nor the query.
        assert_eq!(ids(&a, "v3", "abcdefgh").await, ["long"]);
        assert!(
            ids(&a, "lim", "abcdefgh").await.is_empty(),
            "folded {folded}"
        );
        assert_eq!(ids(&a, "lim", "abcde").await, ["short"]);
        assert_eq!(ids(&a, "lim", "y").await, ["long"]);
    }

    let (s, b) = send(&a, "GET", "/v1/indexes/lim", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let declared = &b["schema"]["full_text_search"];
    assert_eq!(declared["tokenizer"], "word_v3", "{b}");
    assert_eq!(declared["max_token_length"], 5, "{b}");
    let (s, b) = send(&a, "GET", "/v1/indexes/v2", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let declared = &b["schema"]["full_text_search"];
    assert_eq!(declared["tokenizer"], "word_v2", "{b}");
    assert!(declared.get("max_token_length").is_none(), "{b}");

    // A declaration that differs from the stored one.
    for (index, fts) in [
        ("v3", json!({"tokenizer": "word_v4"})),
        ("v3", json!({"tokenizer": "word_v3", "max_token_length": 5})),
        (
            "lim",
            json!({"tokenizer": "word_v3", "max_token_length": 6}),
        ),
        ("lim", json!({"tokenizer": "word_v3"})),
    ] {
        let (s, b) = create(&a, index, &fts).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{index} {fts}: {b}");
        assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    }
    let (s, b) = create(
        &a,
        "lim",
        &json!({"tokenizer": "word_v3", "max_token_length": 5}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "the same declaration: {b}");

    for fts in [
        json!({"tokenizer": "word_v5"}),
        json!({"max_token_length": 0}),
        json!({"max_token_length": 256}),
        json!({"max_token_length": 1.5}),
        json!({"max_token_length": "5"}),
        json!({"max_token_length": 5.0}),
        json!({"max_token_length": null}),
        json!({"max_token_length": -1}),
    ] {
        let (s, b) = create(&a, "door", &fts).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{fts} was stored: {b}");
    }
    // A limit under `pre_tokenized_array`, on the field's own type: accepted without it.
    for (limit, want) in [(None, StatusCode::OK), (Some(5), StatusCode::BAD_REQUEST)] {
        let mut fts = json!({"tokenizer": "pre_tokenized_array", "case_sensitive": true});
        if let Some(n) = limit {
            fts["max_token_length"] = json!(n);
        }
        let body = json!({
            "durability": "durable",
            "schema": {"tags": {"type": "[]string", "full_text_search": fts}},
            "documents": [{"id": "x", "vector": [1.0, 0.5], "attributes": {"tags": ["a b"]}}],
        });
        let index = format!("pre{}", limit.unwrap_or(0));
        let (s, b) = send(&a, "PUT", &format!("/v1/indexes/{index}/documents"), &body).await;
        assert_eq!(s, want, "{body}: {b}");
    }
    for t in ["word_v0", "word_v4"] {
        let (s, b) = create(&a, t, &json!({"tokenizer": t, "max_token_length": 255})).await;
        assert_eq!(s, StatusCode::OK, "{t}: {b}");
    }
}
