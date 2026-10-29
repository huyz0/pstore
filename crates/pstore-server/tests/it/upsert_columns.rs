//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Column upserts and `null` in an upsert (M174): a batch in columns is the batch in rows,
//! and an upsert's `null` is an attribute the row does not hold.

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
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "37")
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

async fn put(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    send(api, "PUT", &format!("/v1/indexes/{index}/documents"), body).await
}

/// A durable write that must succeed.
async fn write(api: &A, index: &str, mut body: Value) -> Value {
    if body.get("durability").is_none() {
        body["durability"] = json!("durable");
    }
    let (s, b) = put(api, index, &body).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
    b
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn query(api: &A, index: &str, q: &Value) -> (StatusCode, Value) {
    send(api, "POST", &format!("/v1/indexes/{index}/query"), q).await
}

/// Every row of `index`, by id, with its attributes.
async fn rows(api: &A, index: &str) -> serde_json::Map<String, Value> {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 1000, "include_attributes": true});
    let (s, b) = query(api, index, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                r["attributes"].clone(),
            )
        })
        .collect()
}

/// Criterion 2: one batch in rows and in columns answers the same, by vector and by BM25.
#[tokio::test]
async fn a_batch_in_columns_answers_as_the_batch_in_rows() {
    let a = api();
    let schema = json!({"owner": "uuid"});
    let u = "6f1e2d3c-4b5a-4968-8776-655443322110";
    write(
        &a,
        "rows",
        json!({"schema": schema, "documents": [
            {"id": "a", "vector": [1.0, 0.5], "text": "the quick brown fox",
             "attributes": {"n": 1, "f": 0.5, "tags": ["x", "y"], "owner": u}},
            {"id": "b", "vector": [0.5, 1.0], "text": "a lazy brown dog",
             "attributes": {"n": null, "f": 2.5, "owner": u}},
            {"id": "c", "vector": [0.0, 1.0], "attributes": {"n": 3, "flag": true}},
        ]}),
    )
    .await;
    write(
        &a,
        "cols",
        json!({"schema": schema, "upsert_columns": {
            "id": ["a", "b", "c"],
            "vector": [[1.0, 0.5], [0.5, 1.0], [0.0, 1.0]],
            "text": ["the quick brown fox", "a lazy brown dog", null],
            "n": [1, null, 3],
            "f": [0.5, 2.5, null],
            "tags": [["x", "y"], null, null],
            "owner": [u, u, null],
            "flag": [null, null, true],
        }}),
    )
    .await;
    fold(&a).await;
    let by_vector = json!({"vector": [1.0, 0.0], "top_k": 10, "include_attributes": true});
    let by_text = json!({"text": "brown", "top_k": 10});
    for q in [by_vector, by_text] {
        let (s, r) = query(&a, "rows", &q).await;
        assert_eq!(s, StatusCode::OK, "{r}");
        let (s, c) = query(&a, "cols", &q).await;
        assert_eq!(s, StatusCode::OK, "{c}");
        assert!(!r["results"].as_array().unwrap().is_empty(), "{r}");
        assert_eq!(r["results"], c["results"], "{q}");
    }
    let got = rows(&a, "cols").await;
    assert_eq!(
        got["b"],
        json!({"f": 2.5, "owner": u, "text": "a lazy brown dog"})
    );
    assert_eq!(got, rows(&a, "rows").await);
}

/// Criterion 3: `null` in an upsert is an absent attribute; in a patch, a removal.
#[tokio::test]
async fn null_in_an_upsert_is_absent_and_in_a_patch_removes() {
    let a = api();
    write(
        &a,
        "docs",
        json!({"documents": [
            {"id": "x", "vector": [1.0, 0.5], "vectors": null,
             "attributes": {"a": null, "b": 2, "c": 3}},
        ]}),
    )
    .await;
    fold(&a).await;
    assert_eq!(rows(&a, "docs").await["x"], json!({"b": 2, "c": 3}));
    write(
        &a,
        "docs",
        json!({"patch_rows": [{"id": "x", "attributes": {"b": null}}]}),
    )
    .await;
    fold(&a).await;
    assert_eq!(
        rows(&a, "docs").await["x"],
        json!({"c": 3}),
        "omitted is kept"
    );

    let refused = [
        // M175: `[1, null]` holds a null element now, accepted:
        // `null_elements::null_elements_answer_in_place_everywhere`.
        json!({"documents": [{"id": "y", "vector": [1.0, 0.5], "attributes": {"$x": null}}]}),
        json!({"schema": {"at": "datetime"},
               "documents": [{"id": "y", "vector": [1.0, 0.5], "attributes": {"at": null}}]}),
        json!({"schema": {"at": "datetime"},
               "upsert_columns": {"id": ["y"], "vector": [[1.0, 0.5]], "at": [null]}}),
    ];
    for body in refused {
        let (s, b) = put(&a, "docs", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
    }
    fold(&a).await;
    assert!(!rows(&a, "docs").await.contains_key("y"));
}

/// Criterion 4: a refusal refuses the whole request, rows written before it included.
#[tokio::test]
async fn a_bad_column_refuses_the_whole_request() {
    let ok = json!({"id": "ok", "vector": [1.0, 0.5], "attributes": {"n": 1}});
    let cases = [
        (json!([]), "bad_request"),
        (json!({"vector": [[1.0, 0.5]]}), "bad_request"),
        (json!({"id": "a"}), "bad_request"),
        (json!({"id": [1], "vector": [[1.0, 0.5]]}), "bad_request"),
        (
            json!({"id": ["a", "b"], "vector": [[1.0, 0.5]]}),
            "bad_request",
        ),
        (
            json!({"id": ["a"], "vector": [[1.0, 0.5]], "n": 1}),
            "bad_request",
        ),
        (
            json!({"id": ["a"], "vector": [[1.0, 0.5]], "n": [1, 2]}),
            "bad_request",
        ),
        (
            json!({"id": ["a"], "vector": [[1.0, 0.5]], "": [null]}),
            "bad_request",
        ),
        (
            json!({"id": ["a"], "vector": [[1.0, 0.5]], "$x": [null]}),
            "bad_request",
        ),
        (
            json!({"id": ["a"], "vector": [[1.0, 0.5]], "n": [{"k": 1}]}),
            "bad_request",
        ),
        (json!({"id": ["a"], "vector": ["@@"]}), "bad_request"),
        (
            json!({"id": ["a", "b"], "vector": [[1.0, 0.5], null]}),
            "schema_conflict",
        ),
    ];
    for (cols, code) in cases {
        let a = api();
        let body = json!({"durability": "durable", "documents": [ok], "upsert_columns": cols});
        let (s, b) = put(&a, "docs", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{cols} -> {b}");
        assert_eq!(b["error"]["code"], code, "{cols} -> {b}");
        // Nothing buffered: the index does not exist, unfolded or folded.
        let (s, _) = query(&a, "docs", &json!({"vector": [1.0, 0.5]})).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{cols} buffered a row");
        fold(&a).await;
        let (s, _) = query(&a, "docs", &json!({"vector": [1.0, 0.5]})).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{cols} reached a fold");
    }

    // A vector-free batch of columns into a folded dense index.
    let a = api();
    write(&a, "docs", json!({"documents": [ok]})).await;
    fold(&a).await;
    let body = json!({"durability": "durable",
                      "upsert_columns": {"id": ["v"], "n": [2]}});
    let (s, b) = put(&a, "docs", &body).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    fold(&a).await;
    assert_eq!(rows(&a, "docs").await.keys().collect::<Vec<_>>(), ["ok"]);
}

/// Criterion 5: the column rows follow `documents`, and both are counted.
#[tokio::test]
async fn columns_follow_rows_and_both_are_counted() {
    let a = api();
    let b = write(
        &a,
        "docs",
        json!({
            "documents": [
                {"id": "d", "vector": [1.0, 0.5], "attributes": {"from": "rows"}},
                {"id": "r", "vector": [1.0, 0.5], "attributes": {"from": "rows"}},
            ],
            "upsert_columns": {"id": ["d", "c"], "vector": [[1.0, 0.5], [0.5, 1.0]],
                               "from": ["columns", "columns"]},
        }),
    )
    .await;
    assert_eq!(b["documents_written"], 4, "{b}");
    fold(&a).await;
    let got = rows(&a, "docs").await;
    assert_eq!(got["d"], json!({"from": "columns"}));
    assert_eq!(got["r"], json!({"from": "rows"}));
    assert_eq!(got["c"], json!({"from": "columns"}));
    assert_eq!(got.len(), 3);

    // `{"id": []}` holds no rows: alone, it is the empty write.
    let (s, b) = put(&a, "docs", &json!({"upsert_columns": {"id": []}})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}

/// Criterion 6: every rule that reads a write's documents reads its column rows.
#[tokio::test]
async fn every_rule_reads_the_column_rows() {
    let store = Accounted::new(MemoryStore::new());
    let a = Api::unsecured(store, LaneId(1)).unwrap();
    a.set_applied_writes(true);
    a.set_affected_ids(true);
    write(
        &a,
        "docs",
        json!({"documents": [
            {"id": "old", "vector": [1.0, 0.5], "attributes": {"n": 1}},
            {"id": "kept", "vector": [1.0, 0.5], "attributes": {"n": 5}},
        ]}),
    )
    .await;
    fold(&a).await;
    let u = "6f1e2d3c-4b5a-4968-8776-655443322110";
    let b = write(
        &a,
        "docs",
        json!({
            "durability": "applied",
            "affected_ids": true,
            "schema": {"owner": "uuid", "text": {"type": "string", "full_text_search": true}},
            "upsert_condition": ["n", "Eq", 1],
            "upsert_columns": {
                "id": ["old", "kept"],
                "vector": [[1.0, 0.5], [0.5, 1.0]],
                "text": ["red fox", "blue fox"],
                "owner": [u, u],
                "n": [2, 2],
            },
        }),
    )
    .await;
    assert_eq!(b["applied"]["ids"]["upserted"], json!(["old"]), "{b}");
    let got = rows(&a, "docs").await;
    assert_eq!(got["old"], json!({"n": 2, "owner": u, "text": "red fox"}));
    assert_eq!(
        got["kept"],
        json!({"n": 5}),
        "the condition admitted a refused row"
    );
}
