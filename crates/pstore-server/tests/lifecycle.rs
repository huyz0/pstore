//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Listing indexes a page at a time, and what one index reports — M9f.1.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

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
        .header("x-pstore-tenant", "16")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn put(api: &Arc<Api<MemoryStore>>, index: &str, durable: bool) {
    let uri = format!("/v1/indexes/{index}/documents");
    let body = json!({"durability": if durable {"durable"} else {"batched"},
        "documents": [{"id": "x", "vector": [1.0, 2.0]}]});
    let (s, b) = send(api, request("PUT", &uri, Some(&body))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold(api: &Arc<Api<MemoryStore>>) -> u64 {
    let (s, b) = send(api, request("POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["epoch"].as_u64().unwrap()
}

async fn list(api: &Arc<Api<MemoryStore>>, query: &str) -> (StatusCode, Value) {
    send(api, request("GET", &format!("/v1/indexes{query}"), None)).await
}

#[tokio::test]
async fn indexes_list_a_page_at_a_time() {
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    // 250 names, most folded, a few only in memory: the pages span both.
    for i in 0..250 {
        put(&api, &format!("ix{i:03}"), i < 240).await;
    }
    fold(&api).await;
    let mut seen: Vec<String> = Vec::new();
    let mut cursor = String::new();
    let mut pages = 0;
    loop {
        let (s, b) = list(&api, &format!("?page_size=100&cursor={cursor}")).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["cost"]["blob_lists"], 0, "{b}");
        assert!(b["cost"]["blob_reads"].as_u64().unwrap() <= 1, "{b}");
        pages += 1;
        // Bounded: a cursor that stopped applying would return the first page forever.
        assert!(pages <= 10, "the cursor stopped advancing");
        seen.extend(
            b["indexes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned()),
        );
        match b["next_cursor"].as_str() {
            Some(next) => cursor = next.to_owned(),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    let want: Vec<String> = (0..250).map(|i| format!("ix{i:03}")).collect();
    assert_eq!(seen, want, "every name once, in order");
    // A prefix narrows; the page bound is refused past its edges.
    let (_, b) = list(&api, "?prefix=ix24&page_size=5").await;
    assert_eq!(
        b["indexes"],
        json!(["ix240", "ix241", "ix242", "ix243", "ix244"])
    );
    assert_eq!(b["next_cursor"], "ix244");
    let (_, b) = list(&api, "?prefix=ix24&cursor=ix244").await;
    assert_eq!(
        b["indexes"],
        json!(["ix245", "ix246", "ix247", "ix248", "ix249"])
    );
    assert_eq!(b["next_cursor"], Value::Null);
    // Exactly a page left: no cursor to an empty page.
    let (_, b) = list(&api, "?prefix=ix24&page_size=10").await;
    assert_eq!(b["indexes"].as_array().unwrap().len(), 10);
    assert_eq!(b["next_cursor"], Value::Null, "{b}");
    // The bound's upper edge is allowed: one page holds them all.
    let (s, b) = list(&api, "?page_size=1000").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["indexes"].as_array().unwrap().len(), 250);
    assert_eq!(b["next_cursor"], Value::Null);
    for bad in ["?page_size=0", "?page_size=1001", "?page_size=x"] {
        let (s, b) = list(&api, bad).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {b}");
    }
    // No parameters: the first page of 100, as the unpaginated list's callers now get.
    let (_, b) = list(&api, "").await;
    assert_eq!(b["indexes"].as_array().unwrap().len(), 100);
}

#[tokio::test]
async fn an_index_reports_when_its_contents_last_changed() {
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    put(&api, "a", true).await;
    let first = fold(&api).await;
    put(&api, "b", true).await;
    fold(&api).await;
    let (_, a) = send(&api, request("GET", "/v1/indexes/a", None)).await;
    assert_eq!(
        a["updated_epoch"], first,
        "another index's fold moved it: {a}"
    );
    assert_eq!(a["approx_row_count"], 1, "{a}");
    // A delete folded into it moves it (the delete vector is a new key).
    let del = json!({"durability": "durable", "deletes": ["x"]});
    send(&api, request("PUT", "/v1/indexes/a/documents", Some(&del))).await;
    let deleted = fold(&api).await;
    let (_, a) = send(&api, request("GET", "/v1/indexes/a", None)).await;
    assert_eq!(a["updated_epoch"], deleted, "{a}");
    // Only in memory: no segment, no epoch.
    put(&api, "c", false).await;
    let (_, c) = send(&api, request("GET", "/v1/indexes/c", None)).await;
    assert_eq!(c["updated_epoch"], Value::Null, "{c}");
}

#[tokio::test]
async fn rows_another_process_folded_are_no_longer_reported_unfolded() {
    let backend = MemoryStore::new();
    let writer = Api::new(Accounted::new(backend.clone()), LaneId(1)).unwrap();
    let folder = Api::new(Accounted::new(backend), LaneId(2)).unwrap();
    put(&writer, "a", true).await;
    fold(&folder).await;
    // The writer has not queried or folded since; its GET is its only read of HEAD.
    let (_, a) = send(&writer, request("GET", "/v1/indexes/a", None)).await;
    assert_eq!(a["unfolded"], false, "{a}");
    assert_eq!(a["documents"], 1, "{a}");
}

#[tokio::test]
async fn a_list_forgets_what_another_process_folded_away() {
    // Code review of M9f.1: an index only deletes ever touched is listed from this process's
    // flushed batches; once another process folds them, HEAD names no such index, and the
    // list -- which read that HEAD -- must stop naming it too.
    let backend = MemoryStore::new();
    let writer = Api::new(Accounted::new(backend.clone()), LaneId(1)).unwrap();
    let folder = Api::new(Accounted::new(backend), LaneId(2)).unwrap();
    let del = json!({"durability": "durable", "deletes": ["q"]});
    let (s, b) = send(
        &writer,
        request("PUT", "/v1/indexes/ghost/documents", Some(&del)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&folder).await;
    let (_, b) = list(&writer, "").await;
    assert!(
        !b["indexes"].as_array().unwrap().contains(&json!("ghost")),
        "{b}"
    );
    let (s, b) = send(&writer, request("GET", "/v1/indexes/ghost", None)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn delete_removes_an_index_and_refuses_a_missing_one() {
    // M9f.2, through the API.
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    put(&api, "gone", true).await;
    put(&api, "kept", true).await;
    fold(&api).await;
    put(&api, "gone", false).await;
    let (s, b) = send(&api, request("DELETE", "/v1/indexes/gone", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["epoch"].as_u64().is_some(), "{b}");
    let (s, _) = send(&api, request("GET", "/v1/indexes/gone", None)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let q = json!({"vector": [1.0, 2.0]});
    let (s, _) = send(&api, request("POST", "/v1/indexes/gone/query", Some(&q))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, l) = list(&api, "").await;
    assert_eq!(l["indexes"], json!(["kept"]));
    let (s, _) = send(&api, request("POST", "/v1/indexes/kept/query", Some(&q))).await;
    assert_eq!(s, StatusCode::OK);
    // Missing: 404, and nothing committed.
    let (_, before) = send(&api, request("GET", "/v1/indexes/kept", None)).await;
    let (s, b) = send(&api, request("DELETE", "/v1/indexes/never", None)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    let (_, after) = send(&api, request("GET", "/v1/indexes/kept", None)).await;
    assert_eq!(before["epoch"], after["epoch"], "a missing index committed");
}
