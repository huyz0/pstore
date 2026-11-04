//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Null elements in scalar arrays (M175): stored in place, answered in place, matching no
//! comparison, and grouped as a value of their own.

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
type S = Accounted<MemoryStore>;

fn api(store: &S) -> A {
    Api::unsecured(store.clone(), LaneId(1)).unwrap()
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

async fn put(api: &A, body: &Value) -> (StatusCode, Value) {
    let mut body = body.clone();
    body["durability"] = json!("durable");
    send(api, "PUT", "/v1/indexes/docs/documents", &body).await
}

async fn write(api: &A, body: Value) {
    let (s, b) = put(api, &body).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn query(api: &A, q: &Value) -> (StatusCode, Value) {
    send(api, "POST", "/v1/indexes/docs/query", q).await
}

/// Each row's attributes, by id, projecting `names`.
async fn rows(api: &A, names: &[&str]) -> serde_json::Map<String, Value> {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 100, "include_attributes": names});
    let (s, b) = query(api, &q).await;
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

/// The ids a filter admits, sorted.
async fn admitted(api: &A, f: &Value) -> Vec<String> {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 100, "filters": f});
    let (s, b) = query(api, &q).await;
    assert_eq!(s, StatusCode::OK, "{f}: {b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn doc(id: &str, attrs: Value) -> Value {
    json!({"id": id, "vector": [1.0, 0.5], "attributes": attrs})
}

const U: &str = "6F1E2D3C-4B5A-4968-8776-655443322110";
const LOWER: &str = "6f1e2d3c-4b5a-4968-8776-655443322110";

/// Criterion 1: null elements answer in place, unfolded, folded and after a restart, under
/// no type and under a recorded one.
#[tokio::test]
async fn null_elements_answer_in_place_everywhere() {
    let store = S::new(MemoryStore::new());
    let a = api(&store);
    write(
        &a,
        json!({"schema": {"u": "uuid"}, "documents": [
            doc("x", json!({"a": [1, null, "x"], "n": 1})),
            doc("y", json!({"a": [null], "n": 2, "u": [null, U]})),
        ]}),
    )
    .await;
    let first = |got: &serde_json::Map<String, Value>| {
        assert_eq!(got["x"], json!({"a": [1, null, "x"], "n": 1}), "{got:?}");
        assert_eq!(
            got["y"],
            json!({"a": [null], "n": 2, "u": [null, LOWER]}),
            "{got:?}"
        );
    };
    first(&rows(&a, &["a", "n", "u"]).await);
    fold(&a).await;
    first(&rows(&a, &["a", "n", "u"]).await);
    // Another attribute of the same rows, projected alone, reads past the null elements.
    let n = rows(&a, &["n"]).await;
    assert_eq!((&n["x"], &n["y"]), (&json!({"n": 1}), &json!({"n": 2})));

    // `uuid` is recorded now: an undeclared null element meets it, and patches set arrays.
    write(&a, json!({"documents": [doc("z", json!({"u": [null]}))]})).await;
    write(
        &a,
        json!({"patch_rows": [
            {"id": "x", "attributes": {"a": [2, null]}},
            {"id": "y", "attributes": {"u": [null, U]}},
        ]}),
    )
    .await;
    fold(&a).await;
    let after = |got: &serde_json::Map<String, Value>| {
        assert_eq!(got["x"]["a"], json!([2, null]), "{got:?}");
        assert_eq!(got["y"]["u"], json!([null, LOWER]), "{got:?}");
        assert_eq!(got["z"], json!({"u": [null]}), "{got:?}");
    };
    after(&rows(&a, &["a", "u"]).await);
    after(&rows(&api(&store), &["a", "u"]).await);
}

/// Criterion 2: a null element matches no comparison.
#[tokio::test]
async fn a_null_element_matches_nothing() {
    let a = api(&S::new(MemoryStore::new()));
    write(
        &a,
        json!({"documents": [
            doc("p", json!({"a": [1, null]})),
            doc("q", json!({"a": [null]})),
            doc("r", json!({"a": [1]})),
            doc("s", json!({"a": []})),
        ]}),
    )
    .await;
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        assert_eq!(admitted(&a, &json!(["a", "Contains", 1])).await, ["p", "r"]);
        assert_eq!(
            admitted(&a, &json!(["a", "NotContains", 1])).await,
            ["q", "s"]
        );
        assert_eq!(admitted(&a, &json!(["a", "AnyGt", 0])).await, ["p", "r"]);
        assert!(admitted(&a, &json!(["a", "Eq", null])).await.is_empty());
    }
}

/// Criterion 3: a null element is a group key's value of its own, after every other kind.
#[tokio::test]
async fn a_null_element_groups_as_itself_and_last() {
    let a = api(&S::new(MemoryStore::new()));
    write(
        &a,
        json!({"documents": [
            doc("p", json!({"a": [1, null]})),
            doc("q", json!({"a": [1, null]})),
            doc("r", json!({"a": [1, 2]})),
            doc("s", json!({"a": [1]})),
        ]}),
    )
    .await;
    fold(&a).await;
    let q = json!({"aggregate_by": {"n": ["Count", "id"]}, "group_by": ["a"]});
    let (s, b) = query(&a, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["aggregation_groups"],
        json!([{"a": [1], "n": 1}, {"a": [1, 2], "n": 1}, {"a": [1, null], "n": 2}])
    );
}

/// Criterion 4: what is still refused.
#[tokio::test]
async fn what_holds_no_null_element_is_refused() {
    let store = S::new(MemoryStore::new());
    let a = api(&store);
    let writes = [
        json!({"documents": [doc("b", json!({"a": [["x"], null]}))]}),
        json!({"schema": {"u": "uuid"}, "documents": [doc("b", json!({"u": [null, "nope"]}))]}),
        json!({"documents": [{"id": "b", "vector": [1.0, 0.5], "attributes": {"text": ["a", null]}}]}),
    ];
    for body in writes {
        let (s, b) = put(&a, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
    }
    let (s, _) = query(&a, &json!({"vector": [1.0, 0.5]})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "a refused write buffered a row");

    write(
        &a,
        json!({"schema": {"u": "uuid"}, "documents": [doc("x", json!({"u": [U], "a": [1]}))]}),
    )
    .await;
    fold(&a).await;
    let (s, b) = put(
        &a,
        &json!({"patch_rows": [{"id": "x", "attributes": {"u": [2, null]}}]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    for f in [
        json!(["a", "In", [1, null]]),
        json!(["a", "Contains", [1, null]]),
        json!(["a", "AnyGt", null]),
        json!(["a", "Contains", null]),
    ] {
        let q = json!({"rank_by": ["id", "asc"], "filters": f});
        let (s, b) = query(&a, &q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{f}: {b}");
    }
    fold(&a).await;
    assert_eq!(rows(&a, &["u"]).await["x"], json!({"u": [LOWER]}));
}
