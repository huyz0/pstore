//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Array attributes and `Contains`/`ContainsAny` through the API — M9h.2.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::BTreeSet;
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

fn request(method: &str, uri: &str, body: Option<&Value>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "23")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn put(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(
        api,
        request("PUT", "/v1/indexes/docs/documents", Some(body)),
    )
    .await
}

async fn fold(api: &Arc<Api<MemoryStore>>) {
    let (s, b) = send(api, request("POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn query(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(api, request("POST", "/v1/indexes/docs/query", Some(body))).await
}

fn doc(id: &str, attrs: Value) -> Value {
    json!({"id": id, "vector": [1.0, 0.5], "attributes": attrs})
}

/// `(id, v)` pairs as documents holding `v` under `n`; `Null` leaves `n` absent.
fn docs(pairs: &[(&str, Value)]) -> Vec<Value> {
    pairs
        .iter()
        .map(|(id, v)| {
            if v.is_null() {
                doc(id, json!({}))
            } else {
                doc(id, json!({"n": v}))
            }
        })
        .collect()
}

async fn seeded(pairs: &[(&str, Value)], folded: bool) -> Arc<Api<MemoryStore>> {
    let api = api();
    let (s, b) = put(
        &api,
        &json!({"durability": "durable", "documents": docs(pairs)}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    if folded {
        fold(&api).await;
    }
    api
}

async fn admitted(api: &Arc<Api<MemoryStore>>, filter: Value) -> BTreeSet<String> {
    let q = json!({"vector": [1.0, 0.5], "top_k": 100, "filters": filter});
    let (s, b) = query(api, &q).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
async fn arrays_come_back_as_written() {
    let written = json!({"a": {"n": [1, 2.0, 2.5, "a", true]}, "b": {"n": []}});
    for folded in [false, true] {
        let api = api();
        let documents: Vec<Value> = written
            .as_object()
            .unwrap()
            .iter()
            .map(|(id, a)| doc(id, a.clone()))
            .collect();
        let (s, b) = put(
            &api,
            &json!({"durability": "durable", "documents": documents}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        if folded {
            fold(&api).await;
        }
        let q = json!({"vector": [1.0, 0.5], "top_k": 10, "include_attributes": true});
        let (s, b) = query(&api, &q).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let rows = b["results"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            let id = row["id"].as_str().unwrap();
            assert_eq!(row["attributes"], written[id], "folded {folded}: {id}");
        }
    }
}

#[tokio::test]
async fn contains_finds_an_element_by_value() {
    let pairs = [
        ("r", json!(["red", "blue"])),
        ("n", json!([1, 2.0])),
        ("e", json!([])),
        ("s", json!("red")),
        ("x", Value::Null),
    ];
    for folded in [false, true] {
        let api = seeded(&pairs, folded).await;
        let is = |f: Value, want: &[&str]| {
            let api = Arc::clone(&api);
            let want = set(want);
            async move {
                assert_eq!(
                    admitted(&api, f.clone()).await,
                    want,
                    "folded {folded}: {f}"
                );
            }
        };
        is(json!(["n", "Contains", "red"]), &["r"]).await;
        is(json!(["n", "Contains", 2]), &["n"]).await;
        is(json!(["n", "Contains", 1.0]), &["n"]).await;
        is(json!(["n", "ContainsAny", ["blue", 1]]), &["r", "n"]).await;
        is(json!(["n", "ContainsAny", []]), &[]).await;
        is(json!(["n", "NotContains", "red"]), &["n", "e", "s", "x"]).await;
        is(json!(["n", "NotContainsAny", ["red", 2]]), &["e", "s", "x"]).await;
        is(json!(["n", "Eq", "red"]), &["s"]).await;
        is(json!(["n", "In", ["red"]]), &["s"]).await;
    }
}

#[tokio::test]
async fn what_an_array_cannot_hold_is_refused() {
    for attrs in [
        json!({"n": [[1]]}),
        json!({"n": [1, null]}),
        json!({"n": [{"k": 1}]}),
        serde_json::from_str::<Value>(r#"{"n": [9223372036854775808]}"#).unwrap(),
        json!({"text": ["a"]}),
    ] {
        let api = api();
        let (s, b) = put(&api, &json!({"documents": [doc("a", attrs.clone())]})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{attrs} was stored: {b}");
    }
    let api = seeded(&[("a", json!([1]))], false).await;
    for f in [
        json!(["n", "Eq", [1]]),
        json!(["n", "Lt", [1]]),
        json!(["n", "In", [[1]]]),
        json!(["n", "NotIn", [["a"]]]),
        json!(["n", "Contains", [1]]),
        json!(["n", "Contains", null]),
        json!(["n", "Contains", {"k": 1}]),
        json!(["n", "ContainsAny", 1]),
        json!(["n", "ContainsAny", [[1]]]),
        json!(["n", "ContainsAny", [null]]),
    ] {
        let q = json!({"vector": [1.0, 0.5], "filters": f});
        let (s, b) = query(&api, &q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{f} was accepted: {b}");
    }
}

#[tokio::test]
async fn rank_by_puts_arrays_after_strings_ordered_by_id() {
    let pairs = [
        ("q", json!([1])),
        ("t", json!(true)),
        ("none", Value::Null),
        ("s", json!("a")),
        ("p", json!([2])),
        ("i", json!(1)),
    ];
    for folded in [false, true] {
        let api = seeded(&pairs, folded).await;
        let order = |dir: &'static str| {
            let api = Arc::clone(&api);
            async move {
                let q = json!({"rank_by": ["n", dir], "top_k": 100});
                let (s, b) = query(&api, &q).await;
                assert_eq!(s, StatusCode::OK, "{b}");
                b["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["id"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(
            order("asc").await,
            ["t", "i", "s", "p", "q", "none"],
            "folded {folded}"
        );
        assert_eq!(
            order("desc").await,
            ["p", "q", "s", "i", "t", "none"],
            "folded {folded}"
        );
    }
}
