//! M161: a walk of `GET /v1/indexes` pages while another process creates and drops names.
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
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const TENANT: u64 = 161;

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT.to_string())
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The lister, and a second process over the same store that changes the names.
fn pair() -> (A, A) {
    let store = Accounted::new(MemoryStore::new());
    (
        Api::unsecured(store.clone(), LaneId(1)).unwrap(),
        Api::unsecured(store, LaneId(2)).unwrap(),
    )
}

async fn create(api: &A, index: &str) -> u64 {
    let (s, b) = send(
        api,
        "PUT",
        &format!("/v1/indexes/{index}"),
        &json!({"dims": 2, "distance_metric": "cosine_distance"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "create {index}: {b}");
    b["epoch"].as_u64().unwrap()
}

async fn drop_index(api: &A, index: &str) {
    let (s, b) = send(api, "DELETE", &format!("/v1/indexes/{index}"), &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "drop {index}: {b}");
}

/// One page: its names, its cursor, its epoch, and the whole body.
async fn page(
    api: &A,
    cursor: Option<&str>,
    size: usize,
) -> (Vec<String>, Option<String>, u64, Value) {
    let uri = match cursor {
        Some(c) => format!("/v1/indexes?page_size={size}&cursor={c}"),
        None => format!("/v1/indexes?page_size={size}"),
    };
    let (s, b) = send(api, "GET", &uri, &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let names = b["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap().to_owned())
        .collect();
    let next = b["next_cursor"].as_str().map(str::to_owned);
    let epoch = b["epoch"].as_u64().expect("a page reports its epoch");
    (names, next, epoch, b)
}

fn names(range: std::ops::Range<usize>) -> Vec<String> {
    range.map(|i| format!("n{i:02}")).collect()
}

#[tokio::test]
async fn list_walk_under_changes() {
    let (a, b) = pair();
    for n in names(0..12) {
        create(&b, &n).await;
    }
    let (first, cursor, e1, _) = page(&a, None, 5).await;
    assert_eq!(first, names(0..5));
    assert_eq!(cursor.as_deref(), Some("n04"));
    // Between pages: created past the cursor, dropped past it, dropped after its page,
    // created before the cursor, and dropped and created again past it.
    create(&b, "n07x").await;
    drop_index(&b, "n08").await;
    drop_index(&b, "n01").await;
    create(&b, "m").await;
    drop_index(&b, "n10").await;
    create(&b, "n10").await;
    let (second, cursor, e2, _) = page(&a, cursor.as_deref(), 5).await;
    assert!(e2 > e1, "commits came between the pages: {e1} then {e2}");
    let (third, cursor, e3, _) = page(&a, cursor.as_deref(), 5).await;
    assert_eq!(cursor, None);
    assert_eq!(e3, e2, "nothing changed between the last two pages");
    let walk: Vec<String> = [first, second, third].concat();
    let want: Vec<String> = [
        "n00", "n01", "n02", "n03", "n04", "n05", "n06", "n07", "n07x",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .chain(["n09", "n10", "n11"].iter().map(|s| (*s).to_owned()))
    .collect();
    assert_eq!(walk, want);

    // A cursor whose index is gone is still a position.
    let (first, cursor, _, _) = page(&a, None, 4).await;
    assert_eq!(first, ["m", "n00", "n02", "n03"]);
    assert_eq!(cursor.as_deref(), Some("n03"));
    drop_index(&b, "n03").await;
    let (second, _, _, _) = page(&a, cursor.as_deref(), 4).await;
    assert_eq!(second, ["n04", "n05", "n06", "n07"]);
}

#[tokio::test]
async fn list_reports_its_epoch() {
    let (a, b) = pair();
    // No HEAD yet: epoch 0, and a page still one read and no LIST.
    let (none, next, epoch, body) = page(&a, None, 5).await;
    assert_eq!((none.len(), next, epoch), (0, None, 0), "{body}");
    assert_eq!(body["cost"]["blob_reads"], 1, "{body}");
    assert_eq!(body["cost"]["blob_lists"], 0, "{body}");
    let mut last = 0;
    for n in names(0..7) {
        last = create(&b, &n).await;
    }
    // A walk with no change between its pages: one epoch, the commit's, and one listing.
    let (p1, c, e1, body) = page(&a, None, 3).await;
    assert_eq!(e1, last);
    assert_eq!(body["cost"]["blob_reads"], 1, "{body}");
    assert_eq!(body["cost"]["blob_lists"], 0, "{body}");
    let (p2, c, e2, _) = page(&a, c.as_deref(), 3).await;
    let (p3, c, e3, _) = page(&a, c.as_deref(), 3).await;
    assert_eq!(c, None);
    assert_eq!((e2, e3), (e1, e1));
    let (all, _, e_all, _) = page(&a, None, 1000).await;
    assert_eq!([p1, p2, p3].concat(), all);
    assert_eq!(e_all, e1);
}
