//! M277: what a write costs in PUTs, per tenant, on sparse and dense multi-tenant workloads.
//!
//! ⚠️ **Exact counts, read off the request counter through the handler.** Each one is the
//! number cross-tenant WAL bundling (D-41's cohort fan-in, lesson 1 of
//! `docs/comparisons/lessons-from-hsearch-opensearch.md`) would have to beat, so a change to
//! any of them is a change to that decision and must be made here, on purpose.
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
use pstore_blob::{Accounted, BlobStore, Faults, Faulty, MemoryStore, OpClass};
use pstore_server::{Api, FoldPolicy};
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

/// Tenants in each sparse workload: enough that a per-tenant cost cannot hide in a constant.
const TENANTS: u128 = 100;

fn serve() -> (Arc<Api<MemoryStore>>, Accounted<MemoryStore>) {
    let store = Accounted::new(MemoryStore::new());
    let api = Api::unsecured(store.clone(), LaneId(1)).expect("a fencing backend");
    (api, store)
}

/// A store whose every request takes `latency`: what lets writes that arrive while a flush
/// is in flight share its successor's bundle, as they do against a real backend.
fn slow(
    latency: std::time::Duration,
) -> (
    Arc<Api<Faulty<MemoryStore>>>,
    Accounted<Faulty<MemoryStore>>,
) {
    let faults = Faults {
        latency_min: latency,
        latency_max: latency,
        ..Faults::none()
    };
    let store = Accounted::new(Faulty::new(MemoryStore::new(), 1, faults));
    let api = Api::unsecured(store.clone(), LaneId(1)).expect("a fencing backend");
    (api, store)
}

async fn write<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    tenant: u128,
    id: &str,
    durability: &str,
) -> StatusCode {
    let body = json!({"durability": durability,
        "documents": [{"id": id, "vector": [1.0, 0.5]}]});
    let req = Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant.to_string())
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = api.clone().router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{v}");
    status
}

/// Every due tenant folds, whatever its age or size.
fn now() -> FoldPolicy {
    FoldPolicy {
        age: std::time::Duration::ZERO,
        bytes: 0,
        ..FoldPolicy::default()
    }
}

fn puts<S: BlobStore>(store: &Accounted<S>) -> u64 {
    store.total(OpClass::Write)
}

/// What one workload cost, in PUTs (CAS included: S3 prices a conditional PUT as a PUT).
#[derive(Debug, PartialEq, Eq)]
struct Cost {
    /// The first write of each tenant on this process.
    first: u64,
    /// A later write of each tenant, once its lane exists.
    steady: u64,
    /// One fold of each tenant.
    fold: u64,
}

async fn sparse(durability: &str) -> Cost {
    let (api, store) = serve();
    let at = puts(&store);
    for t in 1..=TENANTS {
        write(&api, t, "a", durability).await;
    }
    let first = puts(&store) - at;
    let at = puts(&store);
    for t in 1..=TENANTS {
        write(&api, t, "b", durability).await;
    }
    let steady = puts(&store) - at;
    let at = puts(&store);
    let tick = api.fold_due(&now()).await;
    let fold = puts(&store) - at;
    if durability == "durable" {
        assert_eq!(tick.folded, usize::try_from(TENANTS).unwrap(), "{tick:?}");
    }
    Cost {
        first,
        steady,
        fold,
    }
}

#[tokio::test]
async fn a_sparse_durable_write_costs_one_put_per_tenant_and_a_fold_two() {
    let cost = sparse("durable").await;
    println!("sparse durable, {TENANTS} tenants: {cost:?}");
    assert_eq!(
        cost,
        Cost {
            first: 2 * TENANTS as u64,
            steady: TENANTS as u64,
            fold: 2 * TENANTS as u64,
        }
    );
}

#[tokio::test]
async fn a_sparse_batched_write_costs_nothing_until_its_fold() {
    let cost = sparse("batched").await;
    println!("sparse batched, {TENANTS} tenants: {cost:?}");
    assert_eq!(
        cost,
        Cost {
            first: 0,
            steady: 0,
            fold: 0,
        }
    );
}

/// `n` durable writes in flight at once, each to the tenant `tenant(i)` names: the PUTs
/// they cost once every lane exists.
async fn concurrent<S: BlobStore + 'static>(
    (api, store): (Arc<Api<S>>, Accounted<S>),
    n: u128,
    tenant: impl Fn(u128) -> u128,
) -> u64 {
    for i in 0..n {
        write(&api, tenant(i), "warm", "durable").await;
    }
    let at = puts(&store);
    let writes: Vec<_> = (0..n)
        .map(|i| {
            let api = Arc::clone(&api);
            let t = tenant(i);
            tokio::spawn(async move { write(&api, t, &format!("d{i}"), "durable").await })
        })
        .collect();
    for w in writes {
        w.await.unwrap();
    }
    puts(&store) - at
}

const IN_FLIGHT: u128 = 64;

/// What a PUT takes in these tests: S3's first-byte latency for a small PUT is tens of
/// milliseconds, and the share below depends on there being one in flight.
const PUT: std::time::Duration = std::time::Duration::from_millis(20);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_durable_writes_to_one_tenant_share_bundles() {
    // A tenant's own in-flight cap (M128): more is refused at the door.
    let n = 8;
    let cost = concurrent(slow(PUT), n, |_| 1).await;
    println!("{n} concurrent durable writes, one tenant, 20 ms a request: {cost} PUTs");
    // One flush in flight, every write that arrived meanwhile in the next.
    assert!(cost <= 3, "{cost}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_durable_writes_to_many_tenants_cost_one_put_each() {
    let cost = concurrent(slow(PUT), IN_FLIGHT, |i| i + 1).await;
    println!(
        "{IN_FLIGHT} concurrent durable writes, {IN_FLIGHT} tenants, 20 ms a request: {cost} PUTs"
    );
    // ⚠️ **The figure a cross-tenant bundle would cut**: one lane per tenant, so one PUT per
    // tenant per flush however close together they arrive.
    assert_eq!(cost, IN_FLIGHT as u64);
}
