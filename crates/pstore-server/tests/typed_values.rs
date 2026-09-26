//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Float and bool attributes through the API — M9h.1.

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
        .header("x-pstore-tenant", "19")
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
async fn floats_and_bools_come_back_as_written() {
    let written = json!({
        "a": {"x": 1.5, "b": true},
        "b": {"x": 2.0, "b": false},
        "c": {"x": -0.25, "e": 1e3, "i": 2}
    });
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
        for row in b["results"].as_array().unwrap() {
            let id = row["id"].as_str().unwrap();
            // serde_json keeps `2.0` a float and `2` an integer, so equality checks the type.
            assert_eq!(row["attributes"], written[id], "folded {folded}: {id}");
        }
        assert_eq!(b["results"].as_array().unwrap().len(), 3);
    }
}

#[tokio::test]
async fn numbers_compare_as_numbers_and_exactly() {
    let big = 9_007_199_254_740_993_i64; // 2^53 + 1: no f64 holds it
    let pairs = [
        ("i1", json!(1)),
        ("i2", json!(2)),
        ("f175", json!(1.75)),
        ("f2", json!(2.0)),
        ("f25", json!(2.5)),
        ("t", json!(true)),
        ("f", json!(false)),
        ("s", json!("2")),
        ("big", json!(big)),
        ("bigf", json!(9_007_199_254_740_992.0)),
        ("negz", json!(-0.0)),
        ("zero", json!(0)),
        ("none", Value::Null),
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
        is(
            json!(["And", [["n", "Gt", 1.5], ["n", "Lt", 1000]]]),
            &["i2", "f175", "f2", "f25"],
        )
        .await;
        is(json!(["n", "Eq", 2]), &["i2", "f2"]).await;
        is(json!(["n", "Eq", 2.0]), &["i2", "f2"]).await;
        is(json!(["n", "In", [1, 2.5]]), &["i1", "f25"]).await;
        is(json!(["n", "In", [1, 2]]), &["i1", "i2", "f2"]).await;
        is(json!(["n", "In", [0]]), &["negz", "zero"]).await;
        is(json!(["n", "Eq", 0.0]), &["negz", "zero"]).await;
        is(json!(["n", "Eq", true]), &["t"]).await;
        is(json!(["n", "Eq", 1]), &["i1"]).await;
        is(json!(["n", "Lt", true]), &["f"]).await;
        is(json!(["n", "Gt", 9_007_199_254_740_992.0]), &["big"]).await;
        is(json!(["n", "Eq", 9_007_199_254_740_992.0]), &["bigf"]).await;
        is(json!(["n", "Eq", 0]), &["negz", "zero"]).await;
        is(json!(["n", "Eq", -0.0]), &["negz", "zero"]).await;
        is(
            json!(["n", "NotEq", 2]),
            &[
                "i1", "f175", "f25", "t", "f", "s", "big", "bigf", "negz", "zero", "none",
            ],
        )
        .await;
    }
}

#[tokio::test]
async fn a_float_column_prunes_its_blocks() {
    prunes(|i| json!(f64::from(i) + 0.5)).await;
}

#[tokio::test]
async fn an_int_column_still_prunes_for_either_literal() {
    // Its segment stays untyped (version 1), whose missing float zone must still rule out.
    prunes(|i| json!(i)).await;
}

async fn prunes(x: impl Fn(i32) -> Value) {
    let api = api();
    let documents: Vec<Value> = (0..2000)
        .map(|i| doc(&format!("d{i:05}"), json!({"x": x(i)})))
        .collect();
    let (s, b) = put(
        &api,
        &json!({"durability": "durable", "documents": documents}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&api).await;
    let bytes = |f: Value| {
        let api = Arc::clone(&api);
        async move {
            let q = json!({"rank_by": ["x", "asc"], "top_k": 10, "filters": f});
            let (s, b) = query(&api, &q).await;
            assert_eq!(s, StatusCode::OK, "{q} -> {b}");
            assert_eq!(b["results"][0]["id"], "d00000");
            b["meta"]["cost"]["bytes_read"].as_u64().unwrap()
        }
    };
    let unpruned = bytes(json!(["Not", ["x", "Gte", 64.0]])).await;
    for f in [json!(["x", "Lt", 64.0]), json!(["x", "Lt", 64])] {
        let pruned = bytes(f.clone()).await;
        assert!(
            pruned * 4 <= unpruned,
            "{f} read {pruned} bytes against {unpruned} unpruned"
        );
    }
}

#[tokio::test]
async fn rank_by_orders_bools_then_numbers_then_strings_then_absent() {
    let pairs = [
        ("i2", json!(2)),
        ("s", json!("a")),
        ("t", json!(true)),
        ("f15", json!(1.5)),
        ("none", Value::Null),
        ("i1", json!(1)),
        ("f", json!(false)),
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
            ["f", "t", "i1", "f15", "i2", "s", "none"],
            "folded {folded}"
        );
        assert_eq!(
            order("desc").await,
            ["s", "i2", "f15", "i1", "t", "f", "none"],
            "folded {folded}"
        );
    }
}

#[tokio::test]
async fn what_still_has_no_type_is_refused() {
    let beyond_i64: Value = serde_json::from_str("9223372036854775808").unwrap();
    for v in [
        beyond_i64.clone(),
        json!(u64::MAX),
        json!({"k": 1}),
        json!([1, 2]),
        json!(null),
    ] {
        let api = api();
        let (s, b) = put(&api, &json!({"documents": [doc("a", json!({"n": v}))]})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v} was stored: {b}");
    }
    let api = seeded(&[("a", json!(1))], false).await;
    for f in [
        json!(["n", "Eq", beyond_i64]),
        json!(["n", "Eq", u64::MAX]),
        json!(["n", "Eq", {"k": 1}]),
        json!(["n", "Eq", [1]]),
        json!(["n", "In", [1, [2]]]),
    ] {
        let q = json!({"vector": [1.0, 0.5], "filters": f});
        let (s, b) = query(&api, &q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{f} was accepted: {b}");
    }
}

#[tokio::test]
async fn an_integer_serde_json_parsed_as_a_float_is_stored_as_that_float() {
    // Beyond u64, serde_json hands over an f64 before the server sees the literal (spec
    // review, M4). It is stored as that float, not refused and not made an integer.
    let body: Value = serde_json::from_str(
        r#"{"durability": "durable", "documents": [
            {"id": "a", "vector": [1.0, 0.5], "attributes": {"n": 18446744073709551616}}]}"#,
    )
    .unwrap();
    let api = api();
    let (s, b) = put(&api, &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let q = json!({"vector": [1.0, 0.5], "include_attributes": true});
    let (_, b) = query(&api, &q).await;
    assert_eq!(
        b["results"][0]["attributes"]["n"],
        json!(18_446_744_073_709_551_616.0)
    );
}
