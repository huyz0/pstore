//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M7c.3 — **the criterion that decides whether this is a database or a cache.**
//!
//! Every other test in this crate is satisfiable by a process that holds everything in its
//! own memtable. Only a second instance, with an empty memtable, reading what the first made
//! durable, proves the blob store is the tier.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use std::sync::Arc;
use tower::ServiceExt;

async fn send(api: &Arc<Api<MemoryStore>>, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

fn write(durability: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(format!(
            r#"{{"durability":"{durability}","documents":[{{"id":"kept","vector":[1.0,0.5,-0.25,1.0]}}]}}"#
        )))
        .unwrap()
}

fn query() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/indexes/docs/query")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(r#"{"vector":[1.0,0.5,-0.25,1.0],"top_k":5}"#))
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
async fn a_folded_write_is_visible_to_a_second_server() {
    // One backend, two servers. ⚠️ **Different lanes**, because lanes are single-writer and
    // dense: two processes on one lane both start at sequence zero and overwrite each
    // other's bundles, silently. That is the hazard `PSTORE_LANE` exists to make explicit.
    let backend = MemoryStore::new();
    let first = Api::new(Accounted::new(backend.clone()), LaneId(1)).unwrap();
    let second = Api::new(Accounted::new(backend), LaneId(2)).unwrap();

    let (status, body) = send(&first, write("durable")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["durable"], true);

    // ⚠️ Before the fold: durable, and invisible to anyone else. The witness is named --
    // an empty result set, no unfolded hits, and the second server's own epoch -- rather
    // than an unexplained 404.
    let (status, body) = send(&second, query()).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a flushed but unfolded write was visible to a second process: {body}"
    );
    assert_eq!(body["error"]["code"], "index_not_found");

    let (status, body) = send(&first, fold()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["epoch"].as_u64().unwrap() > 0,
        "the fold committed nothing: {body}"
    );

    // After the fold: the blob store is the tier, and the second process reads it.
    let (status, body) = send(&second, query()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["id"], "kept");
    assert_eq!(
        body["meta"]["unfolded_hits"], 0,
        "the second server has no memtable, so no hit may be unfolded: {body}"
    );
    assert!(
        body["meta"]["cost"]["blob_reads"].as_u64().unwrap() > 1,
        "a query over a folded segment read only HEAD: {body}"
    );
}

#[tokio::test]
async fn a_batched_write_survives_nothing() {
    // ⚠️ The other half, and the reason `durability` is a client parameter rather than a
    // policy: a batched write is acknowledged, queryable here, and in no object at all. A
    // fold cannot make durable what was never flushed.
    let backend = MemoryStore::new();
    let first = Api::new(Accounted::new(backend.clone()), LaneId(1)).unwrap();
    let second = Api::new(Accounted::new(backend), LaneId(2)).unwrap();

    send(&first, write("batched")).await;
    let (status, _) = send(&first, query()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the writer cannot see its own write"
    );

    send(&first, fold()).await;
    let (status, body) = send(&second, query()).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a batched write reached another process: {body}"
    );
}

#[tokio::test]
async fn the_index_list_names_folded_and_unfolded_alike() {
    let backend = MemoryStore::new();
    let api = Api::new(Accounted::new(backend), LaneId(1)).unwrap();
    send(&api, write("batched")).await;
    let list = Request::builder()
        .method("GET")
        .uri("/v1/indexes")
        .header("x-pstore-tenant", "7")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&api, list).await;
    assert_eq!(status, StatusCode::OK);
    // ⚠️ HEAD names nothing yet. An enumeration that reported only HEAD would omit an index
    // this same server answers queries for.
    assert_eq!(body["indexes"][0], "docs", "{body}");
    assert_eq!(body["cost"]["blob_lists"], 0);

    let summary = Request::builder()
        .method("GET")
        .uri("/v1/indexes/docs")
        .header("x-pstore-tenant", "7")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&api, summary).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unfolded"], true);
    assert_eq!(body["segments"], 0, "nothing is folded yet: {body}");
}
