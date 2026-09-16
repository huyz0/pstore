//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M7c.2 — what a request costs, and how fresh its answer is.
//!
//! ⚠️ These are the assertions that make `api-design.md` principle 3 a property rather than a
//! sentence: every number below is read off the per-tenant request counter, through the
//! handler, not from the engine.

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

fn docs(n: usize) -> String {
    let rows: Vec<String> = (0..n)
        .map(|i| format!(r#"{{"id":"d{i}","vector":[{i}.0,0.5,-0.25,1.0]}}"#))
        .collect();
    rows.join(",")
}

fn write(tenant: &str, index: &str, durability: &str, n: usize) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!("/v1/indexes/{index}/documents"))
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant)
        .body(Body::from(format!(
            r#"{{"durability":"{durability}","documents":[{}]}}"#,
            docs(n)
        )))
        .unwrap()
}

fn query(tenant: &str, index: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/indexes/{index}/query"))
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant)
        .body(Body::from(r#"{"vector":[1.0,0.5,-0.25,1.0],"top_k":5}"#))
        .unwrap()
}

#[tokio::test]
async fn a_batched_write_costs_nothing() {
    let api = api();
    let (status, body) = send(&api, write("7", "docs", "batched", 10)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["documents_written"], 10);
    assert_eq!(body["durable"], false);
    // ⚠️ Every class, not just writes: a batched write that reads HEAD "just to be sure"
    // would put a blob request on the hot path of every ingest call.
    for class in ["blob_reads", "blob_writes", "blob_lists"] {
        assert_eq!(body["cost"][class], 0, "a batched write issued a {class}");
    }
}

#[tokio::test]
async fn a_durable_write_costs_its_lane_registration_once_then_one_put() {
    let api = api();
    let (status, body) = send(&api, write("7", "docs", "durable", 10)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["durable"], true);
    // ⚠️ **Two writes on the lane's FIRST flush**: the bundle, plus the one CAS that records
    // the lane exists — one per lane lifetime, not per write. A test that only ever sees the
    // first flush cannot tell those two costs apart, which is why the second half is here.
    assert_eq!(
        body["cost"]["blob_writes"], 2,
        "first flush: bundle + lane registration"
    );
    // ⚠️ **2 reads since M7d, and both are once-per-process rather than per-write**: the lane
    // set, and HEAD for the index schemas. The schema read is what lets a row contradicting
    // its index be refused before it is durable -- without it the only remaining guard is the
    // fold, and a fold that refuses stops the tenant.
    assert_eq!(
        body["cost"]["blob_reads"], 2,
        "first flush reads the lane set and the schemas"
    );
    assert_eq!(body["cost"]["blob_lists"], 0);

    let (_, body) = send(&api, write("7", "docs", "durable", 10)).await;
    assert_eq!(
        body["cost"]["blob_writes"], 1,
        "thereafter: the bundle alone"
    );
    assert_eq!(body["cost"]["blob_reads"], 0, "thereafter: nothing is read");
}

#[tokio::test]
async fn a_write_is_queryable_before_it_is_folded() {
    // The freshness layer, through the API: nothing has been flushed or folded, and the
    // document comes back — with the response saying it came from unfolded rows.
    let api = api();
    send(&api, write("7", "docs", "batched", 3)).await;
    let (status, body) = send(&api, query("7", "docs")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // ⚠️ The SET, not the order: which of three near-identical vectors ranks first is the
    // scorer's business and this milestone does not own it. What it owns is that unfolded
    // rows are searchable at all.
    let mut ids: Vec<&str> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["d0", "d1", "d2"], "{body}");
    assert!(
        body["meta"]["unfolded_hits"].as_u64().unwrap() > 0,
        "an answer from the memtable reported no unfolded hits: {body}"
    );
}

#[tokio::test]
async fn a_memtable_query_reads_head_and_nothing_else() {
    let api = api();
    send(&api, write("7", "docs", "batched", 3)).await;
    let (_, body) = send(&api, query("7", "docs")).await;
    // ⚠️ **Exactly one**: HEAD. The fresh segment lives in a private in-memory store that
    // bills nobody, so a query answered entirely from the memtable touches the tenant's
    // store once — and `exists` must not pay a second read for the same HEAD.
    assert_eq!(body["meta"]["cost"]["blob_reads"], 1, "{body}");
    assert_eq!(body["meta"]["cost"]["blob_lists"], 0);
}

#[tokio::test]
async fn cost_is_per_request_not_cumulative() {
    // ⚠️ The counters are monotone and per-tenant, so a handler reporting them raw would
    // report the tenant's running total — larger on every request, and green in any test
    // that only ever makes one.
    let api = api();
    send(&api, write("7", "docs", "durable", 5)).await;
    let (_, first) = send(&api, query("7", "docs")).await;
    let (_, second) = send(&api, query("7", "docs")).await;
    assert_eq!(
        first["meta"]["cost"]["blob_reads"], second["meta"]["cost"]["blob_reads"],
        "the second identical query reported a different cost: {first} then {second}"
    );
}

#[tokio::test]
async fn two_tenants_do_not_see_each_others_documents() {
    // Same index name, different tenants. Keys are derived from the tenant id, so this is
    // structural — and it is exactly what a defaulted tenant header would destroy.
    let api = api();
    send(&api, write("7", "shared", "batched", 3)).await;
    send(&api, write("8", "shared", "batched", 7)).await;

    let (_, seven) = send(&api, query("7", "shared")).await;
    let (_, eight) = send(&api, query("8", "shared")).await;
    assert_eq!(seven["results"].as_array().unwrap().len(), 3);
    assert_eq!(
        eight["results"].as_array().unwrap().len(),
        5,
        "top_k caps it at 5"
    );

    // And a tenant that wrote nothing has no such index at all.
    let (status, _) = send(&api, query("9", "shared")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn no_endpoint_lists() {
    let api = api();
    send(&api, write("7", "docs", "durable", 4)).await;
    for req in [
        write("7", "docs", "durable", 2),
        query("7", "docs"),
        Request::builder()
            .method("GET")
            .uri("/v1/indexes")
            .header("x-pstore-tenant", "7")
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("GET")
            .uri("/v1/indexes/docs")
            .header("x-pstore-tenant", "7")
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("POST")
            .uri("/v1/admin/fold")
            .header("x-pstore-tenant", "7")
            .body(Body::empty())
            .unwrap(),
    ] {
        let uri = req.uri().to_string();
        let (status, body) = send(&api, req).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let cost = if body.get("cost").is_some() {
            &body["cost"]
        } else {
            &body["meta"]["cost"]
        };
        assert_eq!(cost["blob_lists"], 0, "{uri} listed");
    }
}
