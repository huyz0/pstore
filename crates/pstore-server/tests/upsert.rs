//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Upsert and delete by id through the API — M9c. The newest operation on an id decides it,
//! whether the older one is unfolded or in a segment.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn api() -> Arc<Api<MemoryStore>> {
    Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).expect("a fencing backend")
}

async fn send(api: &Arc<Api<MemoryStore>>, req: Request<Body>) -> (StatusCode, Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn request(method: &str, uri: &str, body: Option<&Value>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "12")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn write(api: &Arc<Api<MemoryStore>>, body: Value) -> Value {
    let (s, b) = send(
        api,
        request("PUT", "/v1/indexes/docs/documents", Some(&body)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

async fn fold(api: &Arc<Api<MemoryStore>>) -> u64 {
    let (s, b) = send(api, request("POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["epoch"].as_u64().unwrap()
}

fn doc(id: &str, v: i64) -> Value {
    json!({"id": id, "vector": [1.0, v as f32 / 100.0], "attributes": {"v": v}})
}

fn upsert(docs: Vec<Value>) -> Value {
    json!({"durability": "durable", "documents": docs})
}

/// Every row a broad query returns, as `(id, v)`, sorted by id.
async fn rows(api: &Arc<Api<MemoryStore>>, extra: Value) -> Vec<(String, i64)> {
    let mut q = json!({"vector": [1.0, 0.5], "top_k": 1000, "include_attributes": ["v"]});
    for (k, v) in extra.as_object().unwrap() {
        q[k] = v.clone();
    }
    let (s, b) = send(api, request("POST", "/v1/indexes/docs/query", Some(&q))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut out: Vec<(String, i64)> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                r["attributes"]["v"].as_i64().unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

fn expect(pairs: &[(&str, i64)]) -> Vec<(String, i64)> {
    pairs.iter().map(|(i, v)| ((*i).to_owned(), *v)).collect()
}

#[tokio::test]
async fn the_newest_version_wins_wherever_the_older_one_is() {
    // Unfolded over unfolded.
    let api = api();
    write(&api, upsert(vec![doc("x", 1), doc("y", 1)])).await;
    write(&api, upsert(vec![doc("x", 2)])).await;
    assert_eq!(rows(&api, json!({})).await, expect(&[("x", 2), ("y", 1)]));
    // Both in one fold.
    fold(&api).await;
    assert_eq!(rows(&api, json!({})).await, expect(&[("x", 2), ("y", 1)]));
    // Unfolded over folded.
    write(&api, upsert(vec![doc("y", 3)])).await;
    assert_eq!(rows(&api, json!({})).await, expect(&[("x", 2), ("y", 3)]));
    // Folded over folded: y=3 lands in a second segment and supersedes y=1 in the first.
    fold(&api).await;
    assert_eq!(rows(&api, json!({})).await, expect(&[("x", 2), ("y", 3)]));
    let (_, stats) = send(&api, request("GET", "/v1/indexes/docs", None)).await;
    assert_eq!(stats["documents"], 2, "{stats}");
}

#[tokio::test]
async fn a_deleted_id_is_gone_until_it_is_written_again() {
    let api = api();
    write(&api, upsert(vec![doc("a", 1), doc("b", 1), doc("c", 1)])).await;
    fold(&api).await;
    // Unfolded delete over folded rows.
    let resp = write(&api, json!({"durability": "durable", "deletes": ["a"]})).await;
    assert_eq!(resp["documents_deleted"], 1);
    assert_eq!(rows(&api, json!({})).await, expect(&[("b", 1), ("c", 1)]));
    // Folded delete over folded rows.
    fold(&api).await;
    assert_eq!(rows(&api, json!({})).await, expect(&[("b", 1), ("c", 1)]));
    let (_, stats) = send(&api, request("GET", "/v1/indexes/docs", None)).await;
    assert_eq!(stats["documents"], 2, "{stats}");
    // A re-upsert brings it back, as the new version.
    write(&api, upsert(vec![doc("a", 9)])).await;
    assert_eq!(
        rows(&api, json!({})).await,
        expect(&[("a", 9), ("b", 1), ("c", 1)])
    );
    fold(&api).await;
    assert_eq!(
        rows(&api, json!({})).await,
        expect(&[("a", 9), ("b", 1), ("c", 1)])
    );
}

#[tokio::test]
async fn time_travel_sees_the_version_that_was_current() {
    let api = api();
    write(&api, upsert(vec![doc("x", 1), doc("z", 1)])).await;
    let before = fold(&api).await;
    write(&api, upsert(vec![doc("x", 2)])).await;
    write(&api, json!({"durability": "durable", "deletes": ["z"]})).await;
    let after = fold(&api).await;
    assert_eq!(
        rows(&api, json!({"as_of": before})).await,
        expect(&[("x", 1), ("z", 1)])
    );
    assert_eq!(
        rows(&api, json!({"as_of": after})).await,
        expect(&[("x", 2)])
    );
}

#[tokio::test]
async fn within_a_request_the_later_duplicate_wins_and_deletes_apply_last() {
    let api = api();
    write(
        &api,
        json!({"documents": [doc("p", 1), doc("p", 2), doc("q", 1)], "deletes": ["q"]}),
    )
    .await;
    assert_eq!(rows(&api, json!({})).await, expect(&[("p", 2)]));
    let (s, b) = send(
        &api,
        request("PUT", "/v1/indexes/docs/documents", Some(&json!({}))),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, b) = send(
        &api,
        request(
            "PUT",
            "/v1/indexes/docs/documents",
            Some(&json!({"documents": [], "deletes": []})),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}

#[tokio::test]
async fn a_mostly_superseded_segment_still_answers_top_k() {
    // 50 documents folded; then the 45 BEST-ranked of them (d05..d49: higher v is nearer
    // [1, 0.5]) overwritten with far-away vectors and folded again. The first segment's top
    // rows are all superseded: a leg that did not widen its limit by the deleted count would
    // come back short, and one that ignored the delete vector would return stale versions.
    let api = api();
    write(
        &api,
        upsert((0..50).map(|i| doc(&format!("d{i:02}"), i)).collect()),
    )
    .await;
    fold(&api).await;
    let moved: Vec<Value> = (5..50)
        .map(|i| json!({"id": format!("d{i:02}"), "vector": [-1.0, 0.0], "attributes": {"v": 100 + i}}))
        .collect();
    write(&api, upsert(moved)).await;
    fold(&api).await;
    let q = json!({"vector": [1.0, 0.5], "top_k": 5, "include_attributes": ["v"]});
    let (_, b) = send(&api, request("POST", "/v1/indexes/docs/query", Some(&q))).await;
    let got: Vec<&str> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    // The five untouched documents, best first.
    assert_eq!(got, ["d04", "d03", "d02", "d01", "d00"], "{b}");
}

#[tokio::test]
async fn unfolded_upserts_of_the_best_rows_still_leave_top_k() {
    // Review of M9c.2: the 5 best-ranked folded rows are moved far away by UNFOLDED upserts.
    // A leg not widened by the shadow would return exactly those 5 candidates, all shadowed,
    // and the answer would be the moved rows instead of the next five.
    let api = api();
    write(
        &api,
        upsert((0..20).map(|i| doc(&format!("d{i:02}"), i)).collect()),
    )
    .await;
    fold(&api).await;
    let moved: Vec<Value> = (15..20)
        .map(|i| json!({"id": format!("d{i:02}"), "vector": [-1.0, 0.0], "attributes": {"v": 100 + i}}))
        .collect();
    write(&api, upsert(moved)).await;
    let q = json!({"vector": [1.0, 0.5], "top_k": 5});
    let (_, b) = send(&api, request("POST", "/v1/indexes/docs/query", Some(&q))).await;
    let got: Vec<&str> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(got, ["d14", "d13", "d12", "d11", "d10"], "{b}");
}

#[tokio::test]
async fn the_shadow_check_reads_the_top_candidates_not_every_widened_one() {
    // Review of M9c.2: each leg is widened by the deleted count, and checking the shadow on
    // every widened candidate read a block per candidate -- here up to a thousand rows' blocks
    // for a `top_k` of 5, because one unrelated row was unfolded.
    let api = api();
    write(
        &api,
        upsert((0..2000).map(|i| doc(&format!("d{i:04}"), i)).collect()),
    )
    .await;
    fold(&api).await;
    let gone: Vec<String> = (0..2000).step_by(2).map(|i| format!("d{i:04}")).collect();
    write(&api, json!({"durability": "durable", "deletes": gone})).await;
    fold(&api).await;
    let q = json!({"vector": [1.0, 0.5], "top_k": 5});
    let (_, clean) = send(&api, request("POST", "/v1/indexes/docs/query", Some(&q))).await;
    write(&api, upsert(vec![doc("elsewhere", -1)])).await;
    let (_, shadowed) = send(&api, request("POST", "/v1/indexes/docs/query", Some(&q))).await;
    let bytes = |b: &Value| b["meta"]["cost"]["bytes_read"].as_u64().unwrap();
    // The fresh segment is in memory: the unfolded row costs no bytes, and neither does the
    // check it forces, whose documents the resolve round fetched anyway.
    assert_eq!(
        bytes(&shadowed),
        bytes(&clean),
        "one unfolded row changed what a query reads"
    );
}
