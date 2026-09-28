//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Patches and conditional writes (M13.1): resolved at the fold, against the version the
//! fold's order puts before them, and invisible until it.

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

fn apis(lanes: &[u64]) -> Vec<A> {
    let store = Accounted::new(MemoryStore::new());
    lanes
        .iter()
        .map(|l| Api::new(store.clone(), LaneId(*l)).unwrap())
        .collect()
}

async fn send(
    api: &A,
    method: &str,
    uri: &str,
    body: &Value,
    token: Option<&str>,
) -> (StatusCode, Value, Option<String>) {
    let mut r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "37");
    if let Some(t) = token {
        r = r.header("x-pstore-session", t);
    }
    let res = Arc::clone(api)
        .router()
        .oneshot(r.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let token = res
        .headers()
        .get("x-pstore-session")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        token,
    )
}

/// A durable write of `body`'s operations; returns the response and its session token.
async fn write(api: &A, mut body: Value) -> (Value, String) {
    body["durability"] = json!("durable");
    let (s, b, t) = send(api, "PUT", "/v1/indexes/docs/documents", &body, None).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
    (b, t.unwrap())
}

async fn fold(api: &A) -> Value {
    let (s, b, _) = send(api, "POST", "/v1/admin/fold", &json!({}), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

/// Every row, by id, with its attributes: `{id: attributes}`.
async fn rows(api: &A) -> serde_json::Map<String, Value> {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 1000, "include_attributes": true});
    let (s, b, _) = send(api, "POST", "/v1/indexes/docs/query", &q, None).await;
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

fn doc(id: &str, attrs: Value) -> Value {
    json!({"id": id, "vector": [1.0, 0.5], "attributes": attrs})
}

async fn seeded(api: &A) {
    write(api, json!({"documents": [doc("x", json!({"a": 1, "b": 2, "c": "s"})), doc("y", json!({"a": 3}))]})).await;
    fold(api).await;
}

#[tokio::test]
async fn a_patch_merges_into_the_folded_row() {
    let w = apis(&[1]);
    let a = &w[0];
    seeded(a).await;
    write(
        a,
        json!({"patch_rows": [
            {"id": "x", "attributes": {"a": 5, "b": null, "d": true}},
            {"id": "nope", "attributes": {"a": 1}},
        ]}),
    )
    .await;
    fold(a).await;
    let got = rows(a).await;
    assert_eq!(got["x"], json!({"a": 5, "c": "s", "d": true}));
    assert_eq!(got["y"], json!({"a": 3}));
    assert!(!got.contains_key("nope"), "a patch of nothing made a row");
    assert_eq!(got.len(), 2);
    let find = |v: i64| json!({"rank_by": ["id", "asc"], "filters": ["a", "Eq", v]});
    let (_, b, _) = send(a, "POST", "/v1/indexes/docs/query", &find(5), None).await;
    assert_eq!(b["results"][0]["id"], "x");
    let (_, b, _) = send(a, "POST", "/v1/indexes/docs/query", &find(1), None).await;
    assert_eq!(b["results"], json!([]));
    // The columnar form is the same patch.
    write(
        a,
        json!({"patch_columns": {"id": ["x", "y"], "a": [7, null]}}),
    )
    .await;
    fold(a).await;
    let got = rows(a).await;
    assert_eq!(got["x"]["a"], 7);
    assert_eq!(got["y"], json!({}));
}

#[tokio::test]
async fn operations_apply_in_fold_order() {
    let w = apis(&[1]);
    let a = &w[0];
    write(a, json!({"documents": [doc("w", json!({"k": 1}))]})).await;
    fold(a).await;
    // One fold: an upsert, two patches of it; a patch then an upsert; a delete then a patch.
    write(a, json!({"documents": [doc("u", json!({"k": 0}))]})).await;
    write(
        a,
        json!({"patch_rows": [{"id": "u", "attributes": {"p": 1}}]}),
    )
    .await;
    write(
        a,
        json!({"patch_rows": [{"id": "u", "attributes": {"q": 2}}]}),
    )
    .await;
    write(
        a,
        json!({"patch_rows": [{"id": "v", "attributes": {"p": 1}}]}),
    )
    .await;
    write(a, json!({"documents": [doc("v", json!({"z": 1}))]})).await;
    write(a, json!({"deletes": ["w"]})).await;
    write(
        a,
        json!({"patch_rows": [{"id": "w", "attributes": {"k": 9}}]}),
    )
    .await;
    fold(a).await;
    let got = rows(a).await;
    assert_eq!(got["u"], json!({"k": 0, "p": 1, "q": 2}));
    assert_eq!(
        got["v"],
        json!({"z": 1}),
        "the upsert after the patch overwrites it"
    );
    assert!(
        !got.contains_key("w"),
        "a patch after a delete brought the row back"
    );
}

async fn updated(api: &A) -> Value {
    let (_, b, _) = send(api, "GET", "/v1/indexes/docs", &json!({}), None).await;
    b["updated_epoch"].clone()
}

#[tokio::test]
async fn conditions_decide_against_the_current_version() {
    let w = apis(&[1]);
    let a = &w[0];
    seeded(a).await;
    let yes = json!(["a", "Eq", 1]);
    let no = json!(["a", "Eq", 100]);
    // Refused operations of every kind touch nothing: the segment is not rewritten.
    let before = updated(a).await;
    write(
        a,
        json!({"documents": [doc("x", json!({"a": 50}))], "upsert_condition": no}),
    )
    .await;
    write(
        a,
        json!({"patch_rows": [{"id": "x", "attributes": {"a": 60}}], "patch_condition": no}),
    )
    .await;
    write(a, json!({"deletes": ["x"], "delete_condition": no})).await;
    write(
        a,
        json!({"patch_rows": [{"id": "m", "attributes": {"a": 1}}], "patch_condition": yes}),
    )
    .await;
    fold(a).await;
    assert_eq!(
        updated(a).await,
        before,
        "a refused operation rewrote the segment"
    );
    let got = rows(a).await;
    assert_eq!(got["x"]["a"], 1);
    assert!(!got.contains_key("m"), "a patch of a missing id made a row");
    // Admitted: each applies, and a conditional upsert of a missing id inserts it.
    write(
        a,
        json!({"patch_rows": [{"id": "x", "attributes": {"b": 9}}], "patch_condition": yes}),
    )
    .await;
    write(a, json!({"documents": [doc("x", json!({"a": 2})), doc("m", json!({"a": 7}))], "upsert_condition": yes})).await;
    write(
        a,
        json!({"deletes": ["y"], "delete_condition": ["a", "Eq", 3]}),
    )
    .await;
    fold(a).await;
    let got = rows(a).await;
    assert_eq!(
        got["x"],
        json!({"a": 2}),
        "the upsert replaced the patched row"
    );
    assert_eq!(got["m"], json!({"a": 7}));
    assert!(!got.contains_key("y"));
}

#[tokio::test]
async fn a_deferred_operation_is_invisible_until_its_fold() {
    let w = apis(&[1, 2]);
    let (a, b) = (&w[0], &w[1]);
    seeded(a).await;
    let (_, token) = write(
        a,
        json!({"patch_rows": [{"id": "x", "attributes": {"a": 5}}]}),
    )
    .await;
    for api in [a, b] {
        assert_eq!(
            rows(api).await["x"]["a"],
            1,
            "a patch showed before its fold"
        );
    }
    let q = |c: &str| json!({"rank_by": ["id", "asc"], "consistency": c});
    for api in [a, b] {
        let (s, body, _) = send(
            api,
            "POST",
            "/v1/indexes/docs/query",
            &q("session"),
            Some(&token),
        )
        .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "session: {body}");
    }
    let (s, body, _) = send(a, "POST", "/v1/indexes/docs/query", &q("strong"), None).await;
    assert_eq!(
        s,
        StatusCode::SERVICE_UNAVAILABLE,
        "strong through the writer: {body}"
    );
    fold(a).await;
    let (s, body, _) = send(
        b,
        "POST",
        "/v1/indexes/docs/query",
        &q("session"),
        Some(&token),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(rows(b).await["x"]["a"], 5);
}

async fn dist(api: &A, v: Value) -> f64 {
    let q = json!({"vector": v, "top_k": 1});
    let (s, b, _) = send(api, "POST", "/v1/indexes/docs/query", &q, None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["results"][0]["$dist"].as_f64().unwrap()
}

#[tokio::test]
async fn a_patched_row_keeps_its_stored_vector() {
    for metric in ["cosine_distance", "euclidean_squared"] {
        let w = apis(&[1]);
        let a = &w[0];
        write(
            a,
            json!({"distance_metric": metric, "documents": [
                {"id": "v", "vector": [3.0, 4.0], "attributes": {"k": 1}},
                {"id": "o", "vector": [-4.0, 3.0], "attributes": {"k": 2}}]}),
        )
        .await;
        fold(a).await;
        let before = dist(a, json!([3.0, 4.5])).await;
        write(
            a,
            json!({"patch_rows": [{"id": "v", "attributes": {"k": 3}}]}),
        )
        .await;
        fold(a).await;
        let after = dist(a, json!([3.0, 4.5])).await;
        assert!(
            (before - after).abs() < 1e-5,
            "{metric}: {before} became {after}"
        );
    }
}

#[tokio::test]
async fn a_patch_is_never_a_reject_and_a_conditional_upsert_is_checked() {
    let w = apis(&[1]);
    let a = &w[0];
    seeded(a).await;
    let (_, b, _) = send(a, "GET", "/v1/indexes/docs", &json!({}), None).await;
    let rejected = b["rejected_rows"].as_u64().unwrap();
    write(
        a,
        json!({"patch_rows": [{"id": "x", "attributes": {"a": 8}}]}),
    )
    .await;
    fold(a).await;
    let (_, b, _) = send(a, "GET", "/v1/indexes/docs", &json!({}), None).await;
    assert_eq!(
        b["rejected_rows"].as_u64().unwrap(),
        rejected,
        "a patch counted as a reject"
    );
    // A conditional upsert is a full row: the door refuses a wrong width, as for any upsert.
    // (The fold's reject pass for one that got past a door is the engine's test.)
    let body = json!({"durability": "durable", "upsert_condition": ["a", "Eq", 8],
        "documents": [{"id": "x", "vector": [1.0, 0.5, 9.0]}]});
    let (s, b, _) = send(a, "PUT", "/v1/indexes/docs/documents", &body, None).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "a conditional upsert of the wrong width: {b}"
    );
}

#[tokio::test]
async fn a_plain_fold_reads_what_it_did_before_m13() {
    let w = apis(&[1]);
    let a = &w[0];
    seeded(a).await;
    write(
        a,
        json!({"documents": [doc("z", json!({"a": 1})), doc("x", json!({"a": 4}))]}),
    )
    .await;
    let f = fold(a).await;
    // Measured on the code before M13 (reads 13, bytes 859): a fold with no deferred
    // operation takes no base read.
    assert_eq!(f["cost"]["blob_reads"], 13, "{f}");
    assert_eq!(f["cost"]["bytes_read"], 859, "{f}");
}

#[tokio::test]
async fn a_patch_costs_one_put() {
    let w = apis(&[1]);
    let a = &w[0];
    seeded(a).await;
    let (b, _) = write(
        a,
        json!({"patch_rows": [{"id": "x", "attributes": {"a": 5}}]}),
    )
    .await;
    assert_eq!(b["cost"]["blob_writes"], 1, "{b}");
    assert_eq!(b["cost"]["blob_reads"], 0, "{b}");
    assert_eq!(b["documents_patched"], 1);
}

#[tokio::test]
async fn what_a_patch_cannot_mean_is_refused() {
    let w = apis(&[1]);
    let a = &w[0];
    seeded(a).await;
    for (body, why) in [
        (
            json!({"patch_rows": [{"id": "x", "vector": [1.0, 0.5], "attributes": {}}]}),
            "carries no vector",
        ),
        (
            json!({"patch_rows": [{"id": "x", "extra": 1, "attributes": {}}]}),
            "unknown field extra",
        ),
        (
            json!({"patch_columns": {"id": ["x", "y"], "a": [1]}}),
            "patch_columns",
        ),
        (
            json!({"patch_columns": {"id": ["x"], "vector": [[1.0, 0.5]]}}),
            "vector",
        ),
        (
            json!({"patch_rows": [{"id": "x", "attributes": {"a": 1}}], "patch_condition": deep()}),
            "nests too deeply",
        ),
        (
            json!({"patch_rows": [{"id": "x", "attributes": {"a": 1}}], "patch_condition": "a"}),
            "filter",
        ),
        (
            json!({"documents": [doc("x", json!({}))], "patch_condition": ["a", "Eq", 1]}),
            "patch_condition",
        ),
        (
            json!({"patch_rows": [{"id": "x", "attributes": {"a": 1}}], "upsert_condition": ["a", "Eq", 1]}),
            "upsert_condition",
        ),
        (
            json!({"documents": [doc("x", json!({}))], "delete_condition": ["a", "Eq", 1]}),
            "delete_condition",
        ),
    ] {
        let mut body = body;
        body["durability"] = json!("durable");
        let (s, b, _) = send(a, "PUT", "/v1/indexes/docs/documents", &body, None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} was accepted: {b}");
        let msg = b["error"]["message"].as_str().unwrap();
        assert!(msg.contains(why), "{body}: {msg:?} does not name {why:?}");
    }
    // Every deferred kind is durable only (M13 sweep: each clause of the rule is its own).
    for batched in [
        json!({"durability": "batched", "patch_rows": [{"id": "x", "attributes": {"a": 1}}]}),
        json!({"durability": "batched", "documents": [doc("x", json!({}))], "upsert_condition": ["a", "Eq", 1]}),
        json!({"durability": "batched", "deletes": ["x"], "delete_condition": ["a", "Eq", 1]}),
    ] {
        let (s, b, _) = send(a, "PUT", "/v1/indexes/docs/documents", &batched, None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{batched}: {b}");
        assert!(
            b["error"]["message"].as_str().unwrap().contains("durable"),
            "{b}"
        );
    }
}

/// A filter nested deeper than a condition's encoding carries.
fn deep() -> Value {
    let mut f = json!(["a", "Eq", 1]);
    for _ in 0..80 {
        f = json!(["Not", f]);
    }
    f
}

#[tokio::test]
async fn a_missing_id_meets_each_kind_as_the_table_says() {
    let w = apis(&[1]);
    let a = &w[0];
    seeded(a).await;
    let before = updated(a).await;
    // A patch and a conditional delete of an id with no version do nothing, and a patch that
    // changes nothing touches nothing: the segment is not rewritten.
    write(
        a,
        json!({"patch_rows": [{"id": "gone", "attributes": {"a": 1}}]}),
    )
    .await;
    write(
        a,
        json!({"deletes": ["gone"], "delete_condition": ["a", "Eq", 1]}),
    )
    .await;
    write(
        a,
        json!({"patch_rows": [{"id": "x", "attributes": {"a": 1, "zz": null}}]}),
    )
    .await;
    fold(a).await;
    assert_eq!(
        updated(a).await,
        before,
        "an operation on nothing touched something"
    );
    assert_eq!(rows(a).await.len(), 2);
    // A conditional upsert of an id with no version inserts it, whatever the condition.
    write(
        a,
        json!({"documents": [doc("gone", json!({"a": 8}))], "upsert_condition": ["a", "Eq", 100]}),
    )
    .await;
    fold(a).await;
    assert_eq!(rows(a).await["gone"], json!({"a": 8}));
}
