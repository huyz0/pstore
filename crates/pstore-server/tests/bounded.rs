//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `bounded` consistency (M11.2): a read served from a HEAD this process read within the
//! client's tolerance, skipping the HEAD request, and saying how stale it was.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::{Api, FoldPolicy};
use pstore_testkit::depth::DepthCounting;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

type Store = DepthCounting<MemoryStore>;
type A = Arc<Api<Store>>;

struct World {
    depth: Store,
    apis: Vec<A>,
}

fn world(lanes: &[u64]) -> World {
    let depth = DepthCounting::new(MemoryStore::new());
    let apis = lanes
        .iter()
        .map(|l| Api::new(Accounted::new(depth.clone()), LaneId(*l)).unwrap())
        .collect();
    World { depth, apis }
}

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "23")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn write(api: &A, id: &str, durable: bool) {
    let body = json!({
        "durability": if durable { "durable" } else { "batched" },
        "documents": [{"id": id, "vector": [1.0, 0.5], "attributes": {"n": 1}}]
    });
    let (s, b) = send(api, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold(api: &A) {
    let now = FoldPolicy {
        period: Duration::from_secs(1),
        age: Duration::ZERO,
        bytes: 1 << 20,
    };
    api.fold_due(&now).await;
}

fn relevance() -> Value {
    json!({"vector": [1.0, 0.5], "top_k": 100})
}

fn ranked() -> Value {
    json!({"rank_by": ["id", "asc"], "top_k": 100})
}

fn bounded(mut q: Value, ms: u64) -> Value {
    q["consistency"] = json!("bounded");
    q["max_staleness_ms"] = json!(ms);
    q
}

async fn query(api: &A, q: &Value) -> Value {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", q).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b
}

fn ids(b: &Value) -> Vec<String> {
    let mut v: Vec<String> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    v.sort();
    v
}

fn reads(b: &Value) -> u64 {
    b["meta"]["cost"]["blob_reads"].as_u64().unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_hit_skips_head_and_says_how_stale_it_is() {
    let w = world(&[1, 2]);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    write(a, "x", true).await;
    fold(a).await;
    // A miss: reads HEAD, and remembers it.
    let first = query(b, &bounded(relevance(), 5000)).await;
    assert_eq!(first["meta"]["staleness_ms"], 0);
    tokio::time::advance(Duration::from_millis(1000)).await;
    for q in [relevance(), ranked()] {
        w.depth.reset();
        let eventual = query(b, &q).await;
        let eventual_depth = w.depth.depth();
        assert_eq!(eventual["meta"]["staleness_ms"], 0);
        w.depth.reset();
        let hit = query(b, &bounded(q.clone(), 5000)).await;
        assert_eq!(reads(&hit), reads(&eventual) - 1, "{q}: {hit}");
        assert!(w.depth.depth() < eventual_depth, "{q}: no round saved");
        assert_eq!(hit["meta"]["staleness_ms"], 1000, "{q}");
        assert_eq!(hit["meta"]["consistency"], "bounded");
        assert_eq!(ids(&hit), ids(&eventual));
    }
}

#[tokio::test(start_paused = true)]
async fn a_miss_past_the_bound_reads_head() {
    let w = world(&[1, 2]);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    write(a, "x", true).await;
    fold(a).await;
    query(b, &bounded(relevance(), 500)).await;
    tokio::time::advance(Duration::from_millis(501)).await;
    let eventual = query(b, &relevance()).await;
    let miss = query(b, &bounded(relevance(), 500)).await;
    assert_eq!(reads(&miss), reads(&eventual));
    assert_eq!(miss["meta"]["staleness_ms"], 0);
    // And the miss refilled the cache: the next read within the bound is a hit.
    tokio::time::advance(Duration::from_millis(100)).await;
    let hit = query(b, &bounded(relevance(), 500)).await;
    assert_eq!(hit["meta"]["staleness_ms"], 100);
}

#[tokio::test(start_paused = true)]
async fn a_hit_is_stale_and_says_so() {
    let w = world(&[1, 2]);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    write(a, "x", true).await;
    fold(a).await;
    let first = query(b, &bounded(relevance(), 5000)).await;
    write(a, "y", true).await;
    fold(a).await;
    tokio::time::advance(Duration::from_millis(10)).await;
    let hit = query(b, &bounded(relevance(), 5000)).await;
    assert_eq!(ids(&hit), ["x"], "{hit}");
    assert_eq!(hit["meta"]["epoch"], first["meta"]["epoch"]);
    tokio::time::advance(Duration::from_millis(5000)).await;
    let miss = query(b, &bounded(relevance(), 5000)).await;
    assert_eq!(ids(&miss), ["x", "y"]);
}

#[tokio::test(start_paused = true)]
async fn a_hit_still_answers_this_processs_own_writes() {
    let w = world(&[1]);
    let a = &w.apis[0];
    write(a, "x", true).await;
    fold(a).await;
    query(a, &bounded(relevance(), 60_000)).await;
    write(a, "batched", false).await;
    let hit = query(a, &bounded(relevance(), 60_000)).await;
    assert_eq!(ids(&hit), ["batched", "x"]);
    // Folded and pruned: the cached HEAD has not got these rows and the memtable no longer
    // has them, so the read must fetch HEAD rather than lose them.
    write(a, "durable", true).await;
    fold(a).await;
    let after = query(a, &bounded(relevance(), 60_000)).await;
    assert_eq!(ids(&after), ["batched", "durable", "x"], "{after}");
}

#[tokio::test]
async fn what_bounded_cannot_mean_is_refused() {
    let w = world(&[1]);
    let a = &w.apis[0];
    write(a, "x", true).await;
    let mut no_bound = relevance();
    no_bound["consistency"] = json!("bounded");
    let mut no_level = relevance();
    no_level["max_staleness_ms"] = json!(5);
    let mut past = bounded(relevance(), 5);
    past["as_of"] = json!(0);
    let mut cases = vec![no_bound, no_level, past];
    for bad in [
        json!(-1),
        json!("5"),
        json!(1.5),
        json!(null),
        json!(3_600_001),
    ] {
        let mut q = bounded(relevance(), 0);
        q["max_staleness_ms"] = bad;
        cases.push(q);
    }
    for q in cases {
        let (s, b) = send(a, "POST", "/v1/indexes/docs/query", &q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{q} was accepted: {b}");
    }
}

#[tokio::test(start_paused = true)]
async fn its_own_commit_ends_a_hit() {
    let w = world(&[1]);
    let a = &w.apis[0];
    write(a, "x", true).await;
    fold(a).await;
    query(a, &bounded(relevance(), 60_000)).await;
    let (s, b) = send(a, "DELETE", "/v1/indexes/docs", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(
        a,
        "POST",
        "/v1/indexes/docs/query",
        &bounded(relevance(), 60_000),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "a dropped index answered from the cache: {b}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_hit_on_a_reaped_segment_falls_back_to_a_fresh_read() {
    // Each shape in a world of its own: the first fallback refills the cache, so a second
    // query in the same world would hit a HEAD that names nothing reaped.
    for q in [relevance(), ranked()] {
        let w = world(&[1, 2]);
        let (a, b) = (&w.apis[0], &w.apis[1]);
        write(a, "x", true).await;
        fold(a).await;
        query(b, &bounded(q.clone(), 60_000)).await;
        let (s, body) = send(a, "DELETE", "/v1/indexes/docs", &json!({})).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let (s, body) = send(a, "POST", "/v1/admin/gc?retention=0", &json!({})).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert!(
            body["reaped"].as_u64().unwrap() > 0,
            "nothing reaped: {body}"
        );
        let (s, body) = send(
            b,
            "POST",
            "/v1/indexes/docs/query",
            &bounded(q.clone(), 60_000),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{q}: {body}");
    }
}
