//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M235: `GET /v1/indexes/{index}/export.parquet` pages one epoch's rows in id order as
//! Parquet files the reference reader opens, however the index changes between pages.

use ::parquet::file::reader::{FileReader, SerializedFileReader};
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, HeaderMap, Bytes) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "235")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    (
        status,
        headers,
        res.into_body().collect().await.unwrap().to_bytes(),
    )
}

async fn ok(api: &A, method: &str, uri: &str, body: &Value) -> Value {
    let (s, _, b) = send(api, method, uri, body).await;
    assert_eq!(s, StatusCode::OK, "{uri}: {}", String::from_utf8_lossy(&b));
    serde_json::from_slice(&b).unwrap()
}

fn attrs(i: usize) -> Map<String, Value> {
    let mut a = Map::new();
    a.insert("n".into(), json!(i * 3));
    if !i.is_multiple_of(4) {
        a.insert("s".into(), json!(format!("s{}", i % 7)));
    }
    a
}

async fn put(api: &A, ids: impl Iterator<Item = usize>, deletes: &[&str]) -> u64 {
    let documents: Vec<Value> = ids
        .map(|i| json!({"id": format!("d{i:05}"), "vector": [i as f32, 1.0], "attributes": attrs(i)}))
        .collect();
    let mut body = json!({"durability": "durable", "distance_metric": "euclidean_squared", "documents": documents});
    if !deletes.is_empty() {
        body["deletes"] = json!(deletes);
    }
    ok(api, "PUT", "/v1/indexes/x/documents", &body).await;
    ok(api, "POST", "/v1/admin/fold", &json!({})).await["epoch"]
        .as_u64()
        .unwrap()
}

/// One page: its rows as id -> JSON object, its file's key-value metadata, and its headers.
struct Page {
    rows: BTreeMap<String, Map<String, Value>>,
    order: Vec<String>,
    meta: BTreeMap<String, String>,
    epoch: u64,
    next: Option<String>,
}

async fn page(api: &A, query: &str) -> Page {
    let (s, h, b) = send(
        api,
        "GET",
        &format!("/v1/indexes/x/export.parquet?{query}"),
        &Value::Null,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "{query}: {}",
        String::from_utf8_lossy(&b)
    );
    assert_eq!(h["content-type"], "application/vnd.apache.parquet");
    let reader = SerializedFileReader::new(b).unwrap();
    let meta = reader
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .into_iter()
        .flatten()
        .map(|kv| (kv.key.clone(), kv.value.clone().unwrap_or_default()))
        .collect();
    let (mut rows, mut order) = (BTreeMap::new(), Vec::new());
    for row in reader.get_row_iter(None).unwrap() {
        let Value::Object(mut o) = row.unwrap().to_json_value() else {
            panic!("a row is an object")
        };
        o.retain(|_, v| !v.is_null());
        let id = o.remove("id").unwrap().as_str().unwrap().to_owned();
        order.push(id.clone());
        rows.insert(id, o);
    }
    Page {
        rows,
        order,
        meta,
        epoch: h["x-pstore-epoch"].to_str().unwrap().parse().unwrap(),
        next: h
            .get("x-pstore-next-after")
            .map(|v| v.to_str().unwrap().to_owned()),
    }
}

#[tokio::test]
async fn the_parquet_export_pages_one_epoch() {
    let api: A = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    ok(
        &api,
        "PUT",
        "/v1/indexes/x",
        &json!({"dims": 2, "distance_metric": "euclidean_squared"}),
    )
    .await;
    let first = put(&api, 0..25, &[]).await;

    let one = page(&api, "limit=10").await;
    assert_eq!(one.epoch, first);
    assert_eq!(one.meta["pstore.epoch"], first.to_string());
    assert_eq!(one.meta["pstore.index"], "x");
    assert_eq!(one.next.as_deref(), Some("d00009"));
    assert_eq!(one.meta.get("pstore.next_after"), one.next.as_ref());

    // The index changes between pages; the export still reads the epoch it started at.
    let later = put(&api, 100..103, &["d00015"]).await;
    assert!(later > first);

    // The present epoch by default: the delete and the new rows are in it.
    let now = page(&api, "limit=100&include_vectors=true").await;
    assert_eq!((now.epoch, now.next), (later, None));
    assert!(!now.rows.contains_key("d00015"));
    assert_eq!(now.rows.len(), 27);
    assert_eq!(now.rows["d00101"]["vector"], json!([101.0, 1.0]));

    // Rows written and not yet folded are past any epoch an export reads.
    let unfolded = json!({"durability": "batched", "distance_metric": "euclidean_squared", "documents": [
        {"id": "d00005", "vector": [0.0, 0.0], "attributes": {"n": -1}},
        {"id": "d00200", "vector": [0.0, 0.0]},
    ]});
    ok(&api, "PUT", "/v1/indexes/x/documents", &unfolded).await;
    let mut seen = one.order.clone();
    let mut rows = one.rows;
    let mut next = one.next;
    while let Some(after) = next {
        let p = page(&api, &format!("limit=10&as_of={first}&after={after}")).await;
        assert_eq!(p.epoch, first);
        seen.extend(p.order);
        rows.extend(p.rows);
        next = p.next;
    }
    let want: Vec<String> = (0..25).map(|i| format!("d{i:05}")).collect();
    assert_eq!(seen, want, "every row of the epoch once, in id order");
    for (i, id) in want.iter().enumerate() {
        assert_eq!(rows[id], attrs(i), "{id}");
    }
    assert!(
        !rows["d00003"].contains_key("vector"),
        "no vectors unless asked"
    );

    // An exact last page says no next page.
    let exact = page(&api, &format!("limit=25&as_of={first}")).await;
    assert_eq!((exact.rows.len(), exact.next), (25, None));
    assert!(!exact.meta.contains_key("pstore.next_after"));

    for (q, want) in [
        ("limit=0", StatusCode::BAD_REQUEST),
        ("limit=100001", StatusCode::BAD_REQUEST),
        ("include_vectors=yes", StatusCode::BAD_REQUEST),
        ("as_of=soon", StatusCode::BAD_REQUEST),
        ("as_of=1000000", StatusCode::BAD_REQUEST),
    ] {
        let (s, _, b) = send(
            &api,
            "GET",
            &format!("/v1/indexes/x/export.parquet?{q}"),
            &Value::Null,
        )
        .await;
        assert_eq!(s, want, "{q}: {}", String::from_utf8_lossy(&b));
    }
    let (s, _, _) = send(&api, "GET", "/v1/indexes/nope/export.parquet", &Value::Null).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
