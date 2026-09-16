//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! What one API query costs in **sequential** round trips — M7c, after code review.
//!
//! ⚠️ **Written because review measured 19.** Resolving a hit to an id re-opened every
//! segment and read a block from each, serially: `3 + 2 × segments`, so an eight-segment
//! index paid nineteen sequential round trips for one query against a budget of three. Depth
//! that grows with the segment count grows with *elapsed writes*, which is one of the Nevers.
//!
//! Nothing here asserted depth before: `blob_reads > 1` was the closest, and a depth of 19
//! satisfies it. This pins the number, and the number is **flat in the segment count**.

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use pstore_blob::MemoryStore;
use pstore_server::Api;
use pstore_testkit::depth::DepthCounting;
use pstore_types::LaneId;
use std::sync::Arc;
use tower::ServiceExt;

type Store = DepthCounting<MemoryStore>;

async fn send(api: &Arc<Api<Store>>, req: Request<Body>) -> serde_json::Value {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
}

fn write(i: usize) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(format!(
            r#"{{"durability":"durable","documents":[
                {{"id":"d{i}a","vector":[{i}.0,0.5,-0.25,1.0]}},
                {{"id":"d{i}b","vector":[{i}.5,0.5,-0.25,1.0]}}]}}"#
        )))
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

fn query() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/indexes/docs/query")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(r#"{"vector":[1.0,0.5,-0.25,1.0],"top_k":50}"#))
        .unwrap()
}

/// One query's sequential depth against a tenant whose index has `segments` segments.
async fn depth_at(segments: usize) -> (usize, usize) {
    // `DepthCounting` is `Clone` and shares its counters, so the handle the test keeps and
    // the one the server writes through are the same meter.
    let store = DepthCounting::new(MemoryStore::new());
    let api = Api::new(pstore_blob::Accounted::new(store.clone()), LaneId(1)).unwrap();
    for i in 0..segments {
        send(&api, write(i)).await;
        send(&api, fold()).await;
    }
    store.reset();
    let body = send(&api, query()).await;
    assert_eq!(
        body["results"].as_array().map(Vec::len),
        Some(segments * 2),
        "the fixture must really have {segments} segments' worth of rows: {body}"
    );
    (store.depth(), store.requests())
}

#[tokio::test]
async fn a_query_costs_four_round_trips_however_many_segments_it_has() {
    // ⚠️ **Four, not three, and the ledger says so rather than the test being lenient.** The
    // ranking is three — HEAD, the open round, the leg round. Carrying the *ids* is a fourth,
    // because a payload's address cannot be known until the ranking exists. Getting to three
    // is a format change (ids read alongside the vectors the leg already fetches), which is a
    // decision about what a segment stores.
    //
    // ⚠️ What this test exists to catch is not the 4. It is the **slope**: before review,
    // depth was 3 + 2 × segments.
    let (one, _) = depth_at(1).await;
    let (four, _) = depth_at(4).await;
    let (eight, reqs) = depth_at(8).await;

    assert_eq!(one, 4, "one segment: {one}");
    assert_eq!(
        (four, eight),
        (4, 4),
        "depth grew with the segment count: 1 -> {one}, 4 -> {four}, 8 -> {eight}"
    );
    // Requests may grow with segments -- width is free -- and depth may not.
    assert!(
        reqs >= eight,
        "eight segments issued {reqs} requests in {eight} rounds"
    );
}
