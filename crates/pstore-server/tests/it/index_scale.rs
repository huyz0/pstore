//! M169: thousands of active indexes in one tenant, on one server.
//!
//! Every index is written durable, folded and queried; then one index is written and folded
//! among them, a fresh server queries one, and the limit is set to the count. What must not
//! change with the count is the number of requests; what may grow is HEAD's bytes, within a
//! bound per index.
//!
//! ⚠️ The gate scale (`thousands_of_indexes_in_one_tenant`) is `#[ignore]`d and run in release
//! by `scripts/index-scale.sh`, for `depth.sh`'s reason: `cargo mutants` reruns the suite once
//! per mutant. The suite keeps the same assertions at two small counts.
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
use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_engine::Head;
use pstore_server::{Api, FoldPolicy};
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const TENANT: u64 = 169;
/// Rows written to each index: enough for a top-k to name more than one.
const ROWS: usize = 3;
/// HEAD's bound: bytes per index it names, and what it holds besides.
const HEAD_PER_INDEX: usize = 256;
const HEAD_FIXED: usize = 4096;

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT.to_string())
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

fn name(i: usize) -> String {
    format!("i{i:05}")
}

async fn write(api: &A, index: &str) -> (StatusCode, Value) {
    let docs: Vec<Value> = (0..ROWS)
        .map(|j| {
            let x = j as f64;
            json!({"id": format!("{index}-{j}"), "vector": [x.sin(), x.cos()]})
        })
        .collect();
    send(
        api,
        "PUT",
        &format!("/v1/indexes/{index}/documents"),
        &json!({"durability": "durable", "documents": docs}),
    )
    .await
}

async fn query(api: &A, index: &str) -> Value {
    let (s, b) = send(
        api,
        "POST",
        &format!("/v1/indexes/{index}/query"),
        &json!({"vector": [1.0, 0.0], "top_k": 10}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{index}: {b}");
    let ids: Vec<&str> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), ROWS, "{index}: {b}");
    assert!(
        ids.iter().all(|id| id.starts_with(&format!("{index}-"))),
        "{index} answered another index's rows: {ids:?}"
    );
    b
}

async fn fold(api: &A) {
    api.fold_due(&FoldPolicy {
        period: std::time::Duration::from_secs(1),
        age: std::time::Duration::ZERO,
        bytes: 1,
    })
    .await;
}

/// What one count of indexes cost.
#[derive(Debug, PartialEq, Eq)]
struct Counts {
    /// A query's reads and lists from a server that has not served the tenant.
    query_reads: u64,
    query_lists: u64,
    /// One index's durable write and fold, among all of them: reads and writes.
    fold_reads: u64,
    fold_writes: u64,
}

/// Runs criterion 7 at `k` indexes, and answers the counts that must not depend on it.
async fn at(k: usize) -> Counts {
    let store = Accounted::new(MemoryStore::new());
    let api = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    let clock = Instant::now();
    for i in 0..k {
        let (s, b) = write(&api, &name(i)).await;
        assert_eq!(s, StatusCode::OK, "{}: {b}", name(i));
    }
    let wrote = clock.elapsed();
    fold(&api).await;
    let folded = clock.elapsed() - wrote;
    for i in 0..k {
        query(&api, &name(i)).await;
    }
    let queried = clock.elapsed() - wrote - folded;

    // b. A server that has not served the tenant: one query.
    let fresh = Api::unsecured(store.clone(), LaneId(2)).unwrap();
    let b = query(&fresh, &name(k / 2)).await;
    let query_reads = b["meta"]["cost"]["blob_reads"].as_u64().unwrap();
    let query_lists = b["meta"]["cost"]["blob_lists"].as_u64().unwrap();

    // c. One index written and folded again among the others.
    let (reads, writes) = (store.total(OpClass::Read), store.total(OpClass::Write));
    let (s, b) = write(&api, &name(0)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&api).await;
    let fold_reads = store.total(OpClass::Read) - reads;
    let fold_writes = store.total(OpClass::Write) - writes;
    query(&api, &name(0)).await;
    let tenant = TenantId(u128::from(TENANT));
    let head = store
        .as_tenant(tenant)
        .get(&Head::key(tenant))
        .await
        .unwrap()
        .len();
    assert!(
        head <= HEAD_PER_INDEX * k + HEAD_FIXED,
        "HEAD is {head} B at {k} indexes, {} B an index",
        head / k
    );

    // d. The limit at the count.
    let limited = Api::unsecured(store.clone(), LaneId(3)).unwrap();
    limited.limit_indexes(k);
    let (s, b) = write(&limited, "past").await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "index_limit", "{b}");
    let (s, b) = write(&limited, &name(k - 1)).await;
    assert_eq!(s, StatusCode::OK, "{b}");

    let counts = Counts {
        query_reads,
        query_lists,
        fold_reads,
        fold_writes,
    };
    println!(
        "k={k:>5} head={head:>8} B ({:>3} B/index) {counts:?} \
         write={wrote:.1?} fold={folded:.1?} query={queried:.1?} (provisional)",
        head / k
    );
    counts
}

/// Criterion 7 at every count, against the smallest.
async fn holds_across(ks: &[usize]) {
    let mut first = None;
    for &k in ks {
        let c = at(k).await;
        assert_eq!(c.query_lists, 0, "a LIST at {k}");
        match &first {
            None => first = Some((k, c)),
            Some((k0, c0)) => assert_eq!(&c, c0, "requests at {k} indexes differ from {k0}'s"),
        }
    }
}

#[tokio::test]
async fn request_counts_do_not_grow_with_indexes() {
    holds_across(&[2, 40]).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "gate scale: scripts/index-scale.sh runs it in release"]
async fn thousands_of_indexes_in_one_tenant() {
    holds_across(&[50, 1_000, 5_000]).await;
}
