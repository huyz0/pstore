//! Unknown options must not look like successfully applied safety or query controls.
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
use pstore_server::Api;
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

const TENANT: TenantId = TenantId(9);

fn api() -> (Arc<Api<MemoryStore>>, Accounted<MemoryStore>) {
    let store = Accounted::new(MemoryStore::new());
    let api = Api::new(store.clone(), LaneId(1)).unwrap();
    (api, store)
}

async fn send(
    api: &Arc<Api<MemoryStore>>,
    method: &str,
    path: &str,
    raw: &str,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "9")
        .body(Body::from(raw.to_owned()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

fn spend(store: &Accounted<MemoryStore>) -> Vec<(u64, u64)> {
    [
        OpClass::Read,
        OpClass::Write,
        OpClass::List,
        OpClass::Delete,
    ]
    .into_iter()
    .map(|class| (store.count(TENANT, class), store.bytes(TENANT, class)))
    .collect()
}

#[tokio::test]
async fn unknown_request_fields_are_refused_before_engine_access() {
    let mut failures = Vec::new();
    for (method, suffix, raw, field) in [
        (
            "PUT",
            "documents",
            r#"{"documents":[{"id":"a","vector":[1]}],"return_affected_ids":true}"#,
            "return_affected_ids",
        ),
        (
            "PUT",
            "documents",
            r#"{"documents":[{"id":"a","vector":[1]}],"disable_backpressure":true}"#,
            "disable_backpressure",
        ),
        (
            "PUT",
            "documents",
            r#"{"documents":[{"id":"a","vector":[1]}],"durabilty":"durable"}"#,
            "durabilty",
        ),
        (
            "PUT",
            "documents",
            r#"{"documents":[{"id":"a","vector":[1],"attributes":{"n":1},"attrbutes":{"n":2}}]}"#,
            "attrbutes",
        ),
        (
            "PUT",
            "documents",
            r#"{"documents":[],"documents":[{"id":"a","vector":[1]}],"return_affected_ids":true}"#,
            "return_affected_ids",
        ),
        (
            "PUT",
            "documents",
            r#"{"documents":[{"id":"a","vector":[1],"attributes":{"n":1,"n":2},"attrbutes":{}}]}"#,
            "attrbutes",
        ),
        (
            "POST",
            "query",
            r#"{"vector":[1],"compute_attributes":{}}"#,
            "compute_attributes",
        ),
        (
            "POST",
            "query",
            r#"{"vector":[1],"limit":{"total":1}}"#,
            "limit",
        ),
        (
            "POST",
            "query",
            r#"{"vector":[0],"vector":[1],"compute_attributes":{}}"#,
            "compute_attributes",
        ),
        (
            "POST",
            "query",
            r#"{"vector":[1],"\u0063ompute_attributes":{}}"#,
            "compute_attributes",
        ),
        (
            "POST",
            "query",
            r#"{"queries":[{"vector":[1]},{"vector":[1],"limit":1}]}"#,
            "limit",
        ),
    ] {
        let (api, store) = api();
        let before = spend(&store);
        let (status, body) = send(&api, method, &format!("/v1/indexes/docs/{suffix}"), raw).await;
        let after = spend(&store);
        if status != StatusCode::BAD_REQUEST
            || body["error"]["code"] != "bad_request"
            || !body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(field))
            || after != before
        {
            failures.push(format!(
                "{raw}: {status}, {body}; I/O {before:?} -> {after:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn a_refused_batch_leaves_rows_session_and_registry_unchanged() {
    let (api, store) = api();
    let (status, written) = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        r#"{"documents":[{"id":"seed","vector":[1],"attributes":{"n":1}}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    assert_eq!(written["durable"], false);
    let before = spend(&store);
    let (status, rejected) = send(&api, "PUT", "/v1/indexes/docs/documents",
        r#"{"durability":"durable","documents":[{"id":"new","vector":[1]},{"id":"seed","vector":[1],"attrbutes":{"n":2}}]}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{rejected}");
    assert_eq!(spend(&store), before);
    let (status, listed) = send(&api, "GET", "/v1/indexes", "").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["indexes"], json!(["docs"]));
    let (status, got) = send(
        &api,
        "POST",
        "/v1/indexes/docs/query",
        r#"{"vector":[1],"include_attributes":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["results"].as_array().unwrap().len(), 1, "{got}");
    assert_eq!(got["results"][0]["id"], "seed");
    assert_eq!(got["results"][0]["attributes"]["n"], 1);
    assert_eq!(got["meta"]["session"], written["session"]);

    let before = spend(&store);
    let (status, rejected) = send(
        &api,
        "PUT",
        "/v1/indexes/other/documents",
        r#"{"documents":[{"id":"a","vector":[1]}],"return_affected_ids":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{rejected}");
    assert_eq!(spend(&store), before);
    let (_, listed) = send(&api, "GET", "/v1/indexes", "").await;
    assert_eq!(listed["indexes"], json!(["docs"]));
}

#[tokio::test]
async fn nested_options_are_closed_but_data_names_are_open() {
    let (api, store) = api();
    for raw in [
        json!({"vector":[1],"multi":{"field":"late","vectors":[[1]],"typo":1}}),
        json!({"vector":[1],"fusion":{"rrf":{"typo":1}}}),
        json!({"vector":[1],"boost":{"terms":[{"attr":"n","fn":"linear","weight":1}],"typo":1}}),
        json!({"vector":[1],"boost":{"terms":[{"attr":"n","fn":"linear","weight":1,"typo":1}]}}),
        json!({"vector":[1],"diversify":{"lambda":0.5,"typo":1}}),
        json!({"text":"a","highlight":{"typo":1}}),
        json!({"vector":[1],"facets":{"aggregate_by":{"total":["Count","id"]},"typo":1}}),
    ] {
        let before = spend(&store);
        let (status, body) = send(&api, "POST", "/v1/indexes/docs/query", &raw.to_string()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}: {body}");
        assert!(
            body["error"]["message"].as_str().unwrap().contains("typo"),
            "{body}"
        );
        assert_eq!(spend(&store), before);
    }
    for raw in [
        json!({"documents":[{"id":"a","vector":[1]}],"schema":{"text":{"full_text_search":{"typo":1}}}}),
        json!({"documents":[{"id":"a","vector":[1]}],"schema":{"text":{"regex":true,"typo":1}}}),
        json!({"durability":"durable","patch_rows":[{"id":"a","attributes":{},"typo":1}]}),
        json!({"durability":"durable","patch_by_filter":{"filters":["n","Eq",1],"attributes":{},"typo":1}}),
    ] {
        let before = spend(&store);
        let (status, body) =
            send(&api, "PUT", "/v1/indexes/docs/documents", &raw.to_string()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}: {body}");
        assert_eq!(spend(&store), before);
    }
    // These names are data, even when they resemble unsupported request options.
    let (status, written) = send(&api, "PUT", "/v1/indexes/docs/documents",
        r#"{"documents":[{"id":"a","vector":[0],"vector":[1],"vectors":{"limit":[[1]]},"attributes":{"return_affected_ids":7,"n":1,"n":2}}]}"#).await;
    assert_eq!(status, StatusCode::OK, "{written}");
    assert_eq!(written["durable"], false);
    let (status, got) = send(
        &api,
        "POST",
        "/v1/indexes/docs/query",
        r#"{"vector":[0],"vector":[1],"field":"limit","include_attributes":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["meta"]["consistency"], "eventual");
    assert_eq!(
        got["results"][0]["attributes"],
        json!({"return_affected_ids":7,"n":2})
    );
    let (status, folded) = send(&api, "POST", "/v1/admin/fold", "").await;
    assert_eq!(status, StatusCode::OK, "{folded}");
    let (status, aggregate) = send(
        &api,
        "POST",
        "/v1/indexes/docs/query",
        r#"{"aggregate_by":{"limit":["Count","id"]}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{aggregate}");
    assert_eq!(aggregate["aggregations"]["limit"], 1);
}
