//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The schema through the API — M7d.
//!
//! ⚠️ The three questions M6c could not answer for want of a caller: **who sets it** (the
//! first fold, by inference), **may it change** (no, and the refusal names the path), and
//! **what a disagreement does** (it is refused before anything durable, and if it ever
//! reaches a fold it is dropped and counted rather than stopping the tenant).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use std::sync::Arc;
use tower::ServiceExt;

fn api() -> Arc<Api<MemoryStore>> {
    Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).expect("a fencing backend")
}

async fn send(api: &Arc<Api<MemoryStore>>, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

fn write(body: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("x-pstore-tenant", "7")
        .body(Body::empty())
        .unwrap()
}

fn fold() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/admin/fold")
        .header("x-pstore-tenant", "7")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn the_api_reports_the_schema_and_refuses_a_width_change() {
    let api = api();
    let (status, _) = send(
        &api,
        write(
            r#"{"durability":"durable","documents":[
                {"id":"a","vector":[1.0,0.5,-0.25,1.0],"text":"quarterly revenue"}]}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // ⚠️ Before the fold there is no schema: nothing has inferred one, and reporting a guess
    // would make `null` mean two different things.
    let (_, body) = send(&api, get("/v1/indexes/docs")).await;
    assert!(body["schema"].is_null(), "{body}");

    send(&api, fold()).await;
    let (status, body) = send(&api, get("/v1/indexes/docs")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["schema"]["dims"], 4, "{body}");
    assert_eq!(body["schema"]["text_field"], "text", "{body}");
    assert_eq!(body["rejected_rows"], 0);

    // ⚠️ **BACKLOG row 27's case**: a wrong width AFTER a fold, in a process that has read
    // HEAD. M7c could only catch this at the next query.
    let (status, body) = send(
        &api,
        write(r#"{"documents":[{"id":"short","vector":[9.0,9.0]}]}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "schema_conflict");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("width"),
        "the refusal must name which fact was contradicted: {body}"
    );
    assert_eq!(
        body["error"]["retryable"], false,
        "the same rows will be just as wrong the second time"
    );
}

#[tokio::test]
async fn patching_a_schema_is_refused_with_the_migration_path() {
    let api = api();
    let req = Request::builder()
        .method("PATCH")
        .uri("/v1/indexes/docs/schema")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(r#"{"dims":8}"#))
        .unwrap();
    let (status, body) = send(&api, req).await;
    // ⚠️ **Not a 404.** "There is no such route" and "that operation is not permitted, here is
    // what to do instead" are different answers, and only one of them is true.
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "schema_immutable");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("new index"),
        "the refusal must name the path: {message}"
    );

    // And it still refuses a caller who names no tenant, so nothing is learnable by omission.
    let anon = Request::builder()
        .method("PATCH")
        .uri("/v1/indexes/docs/schema")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&api, anon).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "tenant_required");
}
