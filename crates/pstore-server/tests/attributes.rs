//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Attributes through the API — M9a. Written with a document, returned with its result when
//! asked, at no extra request.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn api() -> Arc<Api<MemoryStore>> {
    Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).expect("a fencing backend")
}

async fn send(api: &Arc<Api<MemoryStore>>, req: Request<Body>) -> (StatusCode, Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn request(method: &str, uri: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "9")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn write(body: &Value) -> Request<Body> {
    request("PUT", "/v1/indexes/docs/documents", body)
}

fn query(body: &Value) -> Request<Body> {
    request("POST", "/v1/indexes/docs/query", body)
}

fn fold() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/admin/fold")
        .header("x-pstore-tenant", "9")
        .body(Body::empty())
        .unwrap()
}

/// Three documents, each with attributes of both types, and one with top-level `text`.
fn batch(durability: &str) -> Value {
    json!({
        "durability": durability,
        "documents": [
            {"id": "a", "vector": [1.0, 0.0, 0.0, 0.0],
             "attributes": {"year": 2024, "lang": "en", "neg": -7}},
            {"id": "b", "vector": [0.0, 1.0, 0.0, 0.0],
             "attributes": {"year": i64::MAX, "lang": "fr"}, "text": "bonjour"},
            {"id": "c", "vector": [0.0, 0.0, 1.0, 0.0]}
        ]
    })
}

/// What each document's `attributes` must read back as, `text` included.
fn expected() -> Value {
    json!({
        "a": {"year": 2024, "lang": "en", "neg": -7},
        "b": {"year": i64::MAX, "lang": "fr", "text": "bonjour"},
        "c": {}
    })
}

fn everything(include: Value) -> Value {
    json!({"vector": [1.0, 1.0, 1.0, 0.0], "top_k": 10, "include_attributes": include})
}

/// `id -> attributes` from a query answer, asserting every row carries the key.
fn by_id(body: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for row in body["results"].as_array().expect("results") {
        let id = row["id"].as_str().unwrap().to_owned();
        out.insert(
            id,
            row.get("attributes")
                .cloned()
                .expect("a row with no attributes"),
        );
    }
    Value::Object(out)
}

#[tokio::test]
async fn attributes_come_back_from_unfolded_rows() {
    let api = api();
    let (status, _) = send(&api, write(&batch("batched"))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(&api, query(&everything(json!(true)))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["meta"]["unfolded_hits"], 3);
    assert_eq!(by_id(&body), expected());
}

#[tokio::test]
async fn attributes_survive_a_fold_and_time_travel() {
    let api = api();
    send(&api, write(&batch("durable"))).await;
    let (status, folded) = send(&api, fold()).await;
    assert_eq!(status, StatusCode::OK, "{folded}");
    let (status, body) = send(&api, query(&everything(json!(true)))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["meta"]["unfolded_hits"], 0,
        "the answer must come from the segment"
    );
    assert_eq!(by_id(&body), expected());

    let mut past = everything(json!(true));
    past["as_of"] = folded["epoch"].clone();
    let (status, body) = send(&api, query(&past)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(by_id(&body), expected());
}

#[tokio::test]
async fn a_named_list_returns_only_those_attributes() {
    let api = api();
    send(&api, write(&batch("batched"))).await;
    let (_, body) = send(&api, query(&everything(json!(["lang", "text", "absent"])))).await;
    assert_eq!(
        by_id(&body),
        json!({"a": {"lang": "en"}, "b": {"lang": "fr", "text": "bonjour"}, "c": {}})
    );
}

#[tokio::test]
async fn exclusions_and_an_empty_list() {
    let api = api();
    send(&api, write(&batch("batched"))).await;
    let mut q = everything(json!(true));
    q["exclude_attributes"] = json!(["year", "text"]);
    let (_, body) = send(&api, query(&q)).await;
    assert_eq!(
        by_id(&body),
        json!({"a": {"lang": "en", "neg": -7}, "b": {"lang": "fr"}, "c": {}})
    );
    // Alone, "everything except"; with an explicit `false`, nothing.
    let alone = json!({"vector": [1.0, 1.0, 1.0, 0.0], "exclude_attributes": ["lang", "neg"]});
    let (_, body) = send(&api, query(&alone)).await;
    assert_eq!(
        by_id(&body),
        json!({"a": {"year": 2024}, "b": {"year": i64::MAX, "text": "bonjour"}, "c": {}})
    );
    let mut none = alone.clone();
    none["include_attributes"] = json!(false);
    let (_, body) = send(&api, query(&none)).await;
    assert!(
        body["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r.get("attributes").is_none())
    );
    // Asked with an empty list: the key, empty.
    let (_, body) = send(&api, query(&everything(json!([])))).await;
    assert_eq!(by_id(&body), json!({"a": {}, "b": {}, "c": {}}));
}

#[tokio::test]
async fn text_written_as_an_attribute_is_the_text_field() {
    let api = api();
    let body = json!({"durability": "durable", "documents": [
        {"id": "p", "vector": [1.0, 0.0], "attributes": {"text": "quarterly revenue"}},
        {"id": "q", "vector": [0.0, 1.0], "text": "weather report"}
    ]});
    assert_eq!(send(&api, write(&body)).await.0, StatusCode::OK);
    for fold_first in [false, true] {
        if fold_first {
            send(&api, fold()).await;
        }
        let q = json!({"text": "revenue", "include_attributes": ["text"]});
        let (status, body) = send(&api, query(&q)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(by_id(&body), json!({"p": {"text": "quarterly revenue"}}));
    }
}

#[tokio::test]
async fn distinct_integer_names_do_not_brick_the_fold() {
    // ⚠️ Found at spec review: each integer attribute's name is a zone-map key in the block
    // index, and 300 distinct names made the segment unwritable -- so the fold, which seals
    // it, failed for the whole tenant and kept failing on the same bundles.
    let api = api();
    let documents: Vec<Value> = (0..300)
        .map(|i| {
            json!({"id": format!("d{i}"), "vector": [i as f32, 1.0],
                   "attributes": {format!("score_{i:020}"): i}})
        })
        .collect();
    let body = json!({"durability": "durable", "documents": documents});
    assert_eq!(send(&api, write(&body)).await.0, StatusCode::OK);
    let (status, folded) = send(&api, fold()).await;
    assert_eq!(status, StatusCode::OK, "{folded}");
    let q = json!({"vector": [299.0, 1.0], "top_k": 1, "include_attributes": true});
    let (status, body) = send(&api, query(&q)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["meta"]["unfolded_hits"], 0);
    assert_eq!(
        by_id(&body),
        json!({"d299": {"score_00000000000000000299": 299}})
    );
}

#[tokio::test]
async fn attributes_are_absent_unless_asked_for() {
    let api = api();
    send(&api, write(&batch("batched"))).await;
    for q in [
        json!({"vector": [1.0, 1.0, 1.0, 0.0], "top_k": 10}),
        everything(json!(false)),
    ] {
        let (status, body) = send(&api, query(&q)).await;
        assert_eq!(status, StatusCode::OK);
        let rows = body["results"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        for row in rows {
            let keys: Vec<&String> = row.as_object().unwrap().keys().collect();
            // `$dist` since M9d: the dense leg scored every row. Still no attributes.
            assert_eq!(keys, ["$dist", "id", "score"], "{row}");
        }
    }
}

#[tokio::test]
async fn returning_attributes_costs_no_request_and_no_byte() {
    let api = api();
    send(&api, write(&batch("durable"))).await;
    send(&api, fold()).await;
    let (_, without) = send(&api, query(&everything(json!(false)))).await;
    let (_, with) = send(&api, query(&everything(json!(true)))).await;
    assert!(without["meta"]["cost"]["blob_reads"].as_u64().unwrap() > 0);
    assert_eq!(with["meta"]["cost"], without["meta"]["cost"]);
}

#[tokio::test]
async fn a_value_the_format_cannot_store_is_refused_not_coerced() {
    for (value, name) in [
        // M9h.1 gave `1.5` and `true` a type; `tests/typed_values.rs` covers them.
        (json!([1, 2]), "x"),
        (json!({"k": 1}), "x"),
        (json!(null), "x"),
        (json!(u64::MAX), "x"),
        (json!(1), ""),
        (json!(1), "text"),
        (json!("x"), "id"),
        (json!("twice"), "text"),
    ] {
        let api = api();
        let mut bad =
            json!({"id": "bad", "vector": [0.0, 1.0], "attributes": {name: value.clone()}});
        if value == json!("twice") {
            // `text` given both ways: one field spelled twice.
            bad["text"] = json!("also");
        }
        let body = json!({"documents": [
            {"id": "ok", "vector": [1.0, 0.0], "attributes": {"fine": 1}},
            bad
        ]});
        let (status, err) = send(&api, write(&body)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{name}: {value} was accepted"
        );
        assert_eq!(err["error"]["code"], "bad_request");
        let message = err["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("bad") && message.contains(&format!("`{name}`")),
            "the refusal does not name the document and attribute: {message}"
        );
        // Nothing was buffered: not even the valid first document.
        let (status, _) = send(&api, query(&json!({"vector": [1.0, 0.0]}))).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{name}: {value} left rows behind"
        );
    }
}
