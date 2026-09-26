//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Order by attribute, `offset`, and paging by id through the API — M9e.

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

fn request(method: &str, uri: &str, body: Option<&Value>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "15")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn put(api: &Arc<Api<MemoryStore>>, body: &Value) {
    let (s, b) = send(
        api,
        request("PUT", "/v1/indexes/docs/documents", Some(body)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn admin(api: &Arc<Api<MemoryStore>>, what: &str) {
    let uri = format!("/v1/admin/{what}");
    let (s, b) = send(api, request("POST", &uri, None)).await;
    assert_eq!(s, StatusCode::OK, "{what}: {b}");
}

async fn query(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(api, request("POST", "/v1/indexes/docs/query", Some(body))).await
}

async fn ids(api: &Arc<Api<MemoryStore>>, body: Value) -> Vec<String> {
    let (s, b) = query(api, &body).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn doc(id: &str, k: Value) -> Value {
    let mut d = json!({"id": id, "vector": [1.0, 0.5], "attributes": {}});
    if !k.is_null() {
        d["attributes"]["k"] = k;
    }
    d
}

/// Ten documents whose `k` is an integer, a string, or absent -- ties included.
fn mixed() -> Vec<Value> {
    vec![
        doc("a", json!(3)),
        doc("b", json!("pear")),
        doc("c", Value::Null),
        doc("d", json!(-7)),
        doc("e", json!("apple")),
        doc("f", json!(3)),
        doc("g", Value::Null),
        doc("h", json!("Zebra")),
        doc("i", json!(100)),
        doc("j", json!("pear")),
    ]
}

const ASC: [&str; 10] = ["d", "a", "f", "i", "h", "e", "b", "j", "c", "g"];
const DESC: [&str; 10] = ["b", "j", "e", "h", "i", "a", "f", "d", "c", "g"];

#[tokio::test]
async fn rank_by_orders_integers_then_strings_then_absent() {
    let api = api();
    // Half folded into a segment, half unfolded: the order must not care where a row is.
    let docs = mixed();
    put(
        &api,
        &json!({"durability": "durable", "documents": docs[..5]}),
    )
    .await;
    admin(&api, "fold").await;
    put(&api, &json!({"documents": docs[5..]})).await;
    let asc = ids(&api, json!({"rank_by": ["k", "asc"], "top_k": 100})).await;
    assert_eq!(asc, ASC);
    let desc = ids(&api, json!({"rank_by": ["k", "desc"], "top_k": 100})).await;
    assert_eq!(desc, DESC);
    // All folded (a durable write flushes the batched rows before it), and the past.
    put(&api, &json!({"durability": "durable", "deletes": ["zz"]})).await;
    admin(&api, "fold").await;
    assert_eq!(
        ids(&api, json!({"rank_by": ["k", "asc"], "top_k": 100})).await,
        ASC
    );
    let (_, stats) = send(&api, request("GET", "/v1/indexes/docs", None)).await;
    let now = stats["epoch"].as_u64().unwrap();
    put(&api, &json!({"durability": "durable", "deletes": ["a"]})).await;
    admin(&api, "fold").await;
    let then = ids(
        &api,
        json!({"rank_by": ["k", "desc"], "top_k": 100, "as_of": now}),
    )
    .await;
    assert_eq!(then, DESC);
    let after = ids(&api, json!({"rank_by": ["k", "desc"], "top_k": 100})).await;
    let without_a: Vec<&str> = DESC.iter().copied().filter(|i| *i != "a").collect();
    assert_eq!(after, without_a);
    // By id, and results carry neither score nor $dist.
    let (_, b) = query(
        &api,
        &json!({"rank_by": ["id", "desc"], "top_k": 2, "include_attributes": ["k"]}),
    )
    .await;
    assert_eq!(
        b["results"],
        json!([{"id": "j", "attributes": {"k": "pear"}}, {"id": "i", "attributes": {"k": 100}}])
    );
}

#[tokio::test]
async fn filters_apply_before_offset_and_offset_spans_every_segment() {
    let api = api();
    // Three homes for rows: two segments and the unfolded rows.
    let docs: Vec<Value> = (0..30)
        .map(|i| doc(&format!("d{i:02}"), json!(i)))
        .collect();
    put(
        &api,
        &json!({"durability": "durable", "documents": docs[..10]}),
    )
    .await;
    admin(&api, "fold").await;
    put(
        &api,
        &json!({"durability": "durable", "documents": docs[10..20]}),
    )
    .await;
    admin(&api, "fold").await;
    put(&api, &json!({"documents": docs[20..]})).await;
    // Even k only, descending, skipping 4: 28 26 24 22 | 20 18 16 14 12 ...
    let got = ids(
        &api,
        json!({"rank_by": ["k", "desc"], "filters": ["Or", [
            ["k", "In", [0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28]]]],
            "offset": 4, "top_k": 5}),
    )
    .await;
    assert_eq!(got, ["d20", "d18", "d16", "d14", "d12"]);
}

#[tokio::test]
async fn paging_by_id_visits_every_document_once_while_the_index_changes() {
    let api = api();
    let docs: Vec<Value> = (0..50)
        .map(|i| doc(&format!("d{i:02}"), json!(i)))
        .collect();
    put(&api, &json!({"durability": "durable", "documents": docs})).await;
    admin(&api, "fold").await;
    let mut seen: Vec<(String, Value)> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut page = 0;
    loop {
        let mut q = json!({"rank_by": ["id", "asc"], "top_k": 7, "include_attributes": ["k"]});
        if let Some(c) = &cursor {
            q["filters"] = json!(["id", "Gt", c]);
        }
        let (s, b) = query(&api, &q).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let rows = b["results"].as_array().unwrap().clone();
        if rows.is_empty() {
            break;
        }
        for r in &rows {
            seen.push((
                r["id"].as_str().unwrap().to_owned(),
                r["attributes"]["k"].clone(),
            ));
        }
        cursor = rows.last().map(|r| r["id"].as_str().unwrap().to_owned());
        page += 1;
        // Bounded: a filter that stopped applying would return the first page forever.
        assert!(page <= 20, "the cursor stopped advancing");
        // Between pages, every kind of change the API offers -- each beyond the cursor. A
        // compaction between pages is the engine test's (`--test order`): no route runs one.
        match page {
            1 => put(&api, &json!({"documents": [doc("d99", json!(99))]})).await,
            2 => put(&api, &json!({"durability": "durable", "deletes": ["d40"]})).await,
            3 => {
                put(
                    &api,
                    &json!({"durability": "durable", "documents": [doc("d45", json!(-45))]}),
                )
                .await
            }
            4 | 5 => admin(&api, "fold").await,
            _ => {}
        }
    }
    let ids: Vec<&str> = seen.iter().map(|(i, _)| i.as_str()).collect();
    let mut want: Vec<String> = (0..50)
        .filter(|i| *i != 40)
        .map(|i| format!("d{i:02}"))
        .collect();
    want.push("d99".to_owned());
    assert_eq!(ids, want, "every document exactly once, in id order");
    let d45 = seen.iter().find(|(i, _)| i == "d45").unwrap();
    assert_eq!(d45.1, json!(-45), "the upserted version");
}

#[tokio::test]
async fn rank_by_refuses_what_it_cannot_mean() {
    let api = api();
    put(&api, &json!({"documents": mixed()})).await;
    for body in [
        json!({"rank_by": ["k", "asc"], "vector": [1.0, 0.5]}),
        json!({"rank_by": ["k", "asc"], "text": "x"}),
        json!({"rank_by": ["k", "asc"], "field": "vector"}),
        json!({"rank_by": ["k", "asc"], "exact": true}),
        json!({"vector": [1.0, 0.5], "offset": 1}),
        json!({"rank_by": ["k", "up"]}),
        json!({"rank_by": ["k"]}),
        json!({"rank_by": "k"}),
        json!({"rank_by": [1, "asc"]}),
        json!({"rank_by": ["k", "asc"], "top_k": 9000, "offset": 1001}),
    ] {
        let (s, b) = query(&api, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
    }
    // The bound itself is allowed; and a relevance query still carries `score`.
    let (s, _) = query(
        &api,
        &json!({"rank_by": ["k", "asc"], "top_k": 9000, "offset": 1000}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, b) = query(&api, &json!({"vector": [1.0, 0.5], "top_k": 1})).await;
    assert!(b["results"][0]["score"].is_number(), "{b}");
}

#[tokio::test]
async fn unfolded_deletes_and_upserts_hide_the_folded_rows() {
    // Code review of M9e: every other test folds before it queries, so the shadow -- an
    // unfolded operation hiding an id's folded row -- was untested.
    let api = api();
    put(
        &api,
        &json!({"durability": "durable", "documents": [
        doc("a", json!(3)), doc("b", json!(1)), doc("c", json!(2))]}),
    )
    .await;
    admin(&api, "fold").await;
    put(&api, &json!({"deletes": ["a"]})).await;
    put(&api, &json!({"documents": [doc("b", json!(9))]})).await;
    let (s, r) = query(
        &api,
        &json!({"rank_by": ["k", "asc"], "include_attributes": ["k"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(
        r["results"],
        json!([{"id": "c", "attributes": {"k": 2}}, {"id": "b", "attributes": {"k": 9}}])
    );
    assert_eq!(r["meta"]["unfolded_hits"], 1, "{r}");
    // As a relevance query answers it: an index nothing but unfolded deletes ever touched
    // does not exist.
    let (s, r) = send(
        &api,
        request(
            "PUT",
            "/v1/indexes/ghost/documents",
            Some(&json!({"deletes": ["x"]})),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, r) = send(
        &api,
        request(
            "POST",
            "/v1/indexes/ghost/query",
            Some(&json!({"rank_by": ["id", "asc"]})),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{r}");
}
