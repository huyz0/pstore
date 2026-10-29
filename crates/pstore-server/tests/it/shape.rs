//! M143: a public body's JSON shape is bounded before any parser runs, and a query leg's
//! `top_k` is bounded.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_server::{Api, Auth, RequestLimits};
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn private() -> Auth {
    Auth::from_vars(|k| (k == "PSTORE_ALLOW_UNAUTHENTICATED").then(|| "1".to_owned())).unwrap()
}

/// Depth 4, 4 elements, 3 members, 16-byte strings: small enough to reach by hand.
fn small() -> RequestLimits {
    RequestLimits::builder()
        .json_depth(4)
        .array_elements(4)
        .object_members(3)
        .string_bytes(16)
        .build()
        .unwrap()
}

fn serve(limits: RequestLimits) -> (Arc<Api<MemoryStore>>, Accounted<MemoryStore>) {
    let store = Accounted::new(MemoryStore::new());
    let api = Api::with_limits(store.clone(), LaneId(1), None, private(), limits).unwrap();
    (api, store)
}

fn totals(store: &Accounted<MemoryStore>) -> [u64; 4] {
    [
        OpClass::Read,
        OpClass::Write,
        OpClass::List,
        OpClass::Delete,
    ]
    .map(|c| store.total(c))
}

async fn send(
    api: &Arc<Api<MemoryStore>>,
    method: &str,
    path: &str,
    body: &str,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "1")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = api.clone().router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

const WRITE: &str = "/v1/indexes/docs/documents";
const QUERY: &str = "/v1/indexes/docs/query";

#[tokio::test]
async fn shape_limits_are_exact() {
    // (field, method, path, one past the limit, exactly at it, what the at-limit body gets)
    let cases: [(&str, &str, &str, &str, &str, StatusCode); 8] = [
        (
            "array_elements",
            "PUT",
            WRITE,
            r#"{"deletes":["a","b","c","d","e"]}"#,
            r#"{"deletes":["a","b","c","d"]}"#,
            StatusCode::OK,
        ),
        (
            "object_members",
            "PUT",
            WRITE,
            r#"{"documents":[{"id":"a","vector":[1],"attributes":{"p":1,"q":2,"r":3,"s":4}}]}"#,
            r#"{"documents":[{"id":"a","vector":[1],"attributes":{"p":1,"q":2,"r":3}}]}"#,
            StatusCode::OK,
        ),
        (
            "string_bytes",
            "PUT",
            WRITE,
            r#"{"documents":[{"id":"aaaaaaaaaaaaaaaaa","vector":[1]}]}"#,
            r#"{"documents":[{"id":"aaaaaaaaaaaaaaaa","vector":[1]}]}"#,
            StatusCode::OK,
        ),
        (
            "json_depth",
            "PUT",
            WRITE,
            r#"{"documents":[{"id":"a","vector":[1],"attributes":{"t":[1]}}]}"#,
            r#"{"documents":[{"id":"a","vector":[1],"attributes":{"t":1}}]}"#,
            StatusCode::OK,
        ),
        (
            "array_elements",
            "POST",
            QUERY,
            r#"{"vector":[1],"top_k":1,"filters":["id","In",["a","b","c","d","e"]]}"#,
            r#"{"vector":[1],"top_k":1,"filters":["id","In",["a","b","c","d"]]}"#,
            StatusCode::NOT_FOUND,
        ),
        (
            "object_members",
            "POST",
            QUERY,
            r#"{"vector":[1],"top_k":1,"consistency":"eventual","top_k":1}"#,
            r#"{"vector":[1],"top_k":1,"consistency":"eventual"}"#,
            StatusCode::NOT_FOUND,
        ),
        (
            "string_bytes",
            "POST",
            QUERY,
            r#"{"vector":[1],"top_k":1,"filters":["id","Eq","aaaaaaaaaaaaaaaaa"]}"#,
            r#"{"vector":[1],"top_k":1,"filters":["id","Eq","aaaaaaaaaaaaaaaa"]}"#,
            StatusCode::NOT_FOUND,
        ),
        (
            "json_depth",
            "POST",
            QUERY,
            r#"{"vector":[1],"top_k":1,"filters":["And",[["id","In",["a"]]]]}"#,
            r#"{"vector":[1],"top_k":1,"filters":["And",[["id","Eq","a"]]]}"#,
            StatusCode::NOT_FOUND,
        ),
    ];
    for (field, method, path, over, at, admitted) in cases {
        // The refusal first, on a fresh server: it must build no engine and read nothing.
        let (api, store) = serve(small());
        let (s, b) = send(&api, method, path, over).await;
        assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{field} {path}: {b}");
        assert_eq!(b["error"]["code"], "request_too_large", "{field}");
        assert_eq!(b["error"]["retryable"], false, "{field}");
        let message = b["error"]["message"].as_str().unwrap();
        assert!(message.contains(field), "{field}: {message}");
        assert_eq!(
            api.engines().await,
            0,
            "{field} {path}: an engine was built"
        );
        assert_eq!(
            totals(&store),
            [0; 4],
            "{field} {path}: the store was touched"
        );
        let (s, b) = send(&api, method, path, at).await;
        assert_eq!(s, admitted, "{field} {path} at the limit: {b}");
    }
}

#[tokio::test]
async fn a_breach_is_refused_before_parsing_on_every_route() {
    let (api, store) = serve(small());
    // Too deep, then not JSON at all: the depth is what is reported.
    for path in [
        WRITE,
        QUERY,
        "/v1/indexes/docs/warm",
        "/v1/indexes/docs/external/query",
    ] {
        let method = if path == WRITE { "PUT" } else { "POST" };
        let (s, b) = send(&api, method, path, "[[[[[ not json").await;
        assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{path}: {b}");
        assert!(
            b["error"]["message"]
                .as_str()
                .unwrap()
                .contains("json_depth"),
            "{path}: {b}"
        );
    }
    // Brackets inside a string are text: at default limits this deep-looking id is a string.
    let (s, b) = send(
        &api,
        "PUT",
        WRITE,
        r#"{"documents":[{"id":"[[[[[[","vector":[1]}]}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(api.engines().await, 1);
    assert!(totals(&store)[0] > 0);
}

#[tokio::test]
async fn query_top_k_is_bounded() {
    let (api, _store) = serve(RequestLimits::default());
    let (s, b) = send(
        &api,
        "PUT",
        WRITE,
        r#"{"documents":[{"id":"a","vector":[1]}]}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for body in [
        json!({"vector": [1], "top_k": 10_001}),
        json!({"text": "a", "top_k": 10_001}),
        json!({"queries": [{"vector": [1], "top_k": 10_001}]}),
    ] {
        let (s, b) = send(&api, "POST", QUERY, &body.to_string()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}: {b}");
        assert_eq!(b["error"]["code"], "bad_request");
        assert!(
            b["error"]["message"].as_str().unwrap().contains("10000"),
            "{body}: {b}"
        );
    }
    let (s, b) = send(
        &api,
        "POST",
        QUERY,
        &json!({"vector": [1], "top_k": 10_000}).to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["results"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn existing_caps_keep_their_refusals() {
    let (api, _store) = serve(RequestLimits::default());
    let (s, b) = send(
        &api,
        "PUT",
        WRITE,
        r#"{"documents":[{"id":"a","vector":[1],"attributes":{"n":1}}]}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let queries: Vec<Value> = (0..17)
        .map(|_| json!({"vector": [1], "top_k": 1}))
        .collect();
    let fields: serde_json::Map<String, Value> =
        (0..9).map(|i| (format!("f{i}"), json!([[1]]))).collect();
    for (body, says) in [
        (
            json!({"rank_by": ["n", "asc"], "top_k": 10_001}),
            "for a rank_by order",
        ),
        (
            json!({"vector": [1], "top_k": 1, "facets": {"aggregate_by": {"c": ["Count", "id"]}, "top_k": 10_001}}),
            "facets: top_k may be at most 10000 groups for an aggregation",
        ),
        (json!({"queries": queries}), "1 to 16 queries"),
    ] {
        let (s, b) = send(&api, "POST", QUERY, &body.to_string()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b["error"]["message"].as_str().unwrap().contains(says),
            "{says}: {b}"
        );
    }
    let (s, b) = send(
        &api,
        "PUT",
        WRITE,
        &json!({"documents": [{"id": "b", "vector": [1], "vectors": fields}]}).to_string(),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("named vector fields, at most 8"),
        "{b}"
    );
}
