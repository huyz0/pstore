//! M128: provisional CPU, allocation and body-reservation costs of public ingress.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions and fixture construction report measurement failures"
)]
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_server::{Api, Auth, IngressCounts, RequestLimits};
use pstore_types::LaneId;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use std::alloc::System;
use std::hint::black_box;
use std::time::Instant;
use tower::ServiceExt;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

const BODY: usize = 4096;

type Chunk = Result<bytes::Bytes, std::io::Error>;

fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}

/// A query the engine answers from an empty index, padded to `len` bytes.
fn query(len: usize) -> Vec<u8> {
    let mut v = br#"{"vector":[1],"top_k":1}"#.to_vec();
    v.resize(len, b' ');
    v
}

fn req(body: Body) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/indexes/docs/query")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "9")
        .body(body)
        .unwrap()
}

async fn answer(router: &Router, body: Body) -> (StatusCode, axum::body::Bytes) {
    let res = router.clone().oneshot(req(body)).await.unwrap();
    let status = res.status();
    (status, res.into_body().collect().await.unwrap().to_bytes())
}

async fn measure(
    sample: usize,
    case: &str,
    run: impl AsyncFn() -> (StatusCode, axum::body::Bytes),
) {
    let (status, first) = run().await;
    for _ in 0..200 {
        assert_eq!(run().await, (status, first.clone()));
    }
    let n = 2000;
    let start = Instant::now();
    let region = Region::new(GLOBAL);
    let mut outcome = 0_u64;
    for _ in 0..n {
        let (s, bytes) = run().await;
        assert_eq!(s, status);
        outcome = outcome.wrapping_add(black_box(digest(&bytes)));
    }
    let elapsed = start.elapsed().as_nanos();
    let stats = region.change();
    assert_eq!(outcome, digest(&first).wrapping_mul(n));
    println!(
        "{sample},{case},{},{n},{:.2},{:.2},{:.2},{:.2},{:016x}",
        status.as_u16(),
        elapsed as f64 / n as f64,
        stats.allocations as f64 / n as f64,
        stats.reallocations as f64 / n as f64,
        stats.bytes_allocated as f64 / n as f64,
        digest(&first)
    );
}

/// Run this binary alone so global allocator counters have no competing tests.
#[tokio::test]
#[ignore = "provisional CPU/allocation campaign; run explicitly with --release --ignored --nocapture"]
async fn ingress_cpu_allocations_reservations_and_refusal_digests() {
    let store = Accounted::new(MemoryStore::new());
    let private =
        Auth::from_vars(|k| (k == "PSTORE_ALLOW_UNAUTHENTICATED").then(|| "1".to_owned())).unwrap();
    let limits = RequestLimits::builder()
        .body_bytes(BODY)
        .active_requests(2)
        .tenant_active_requests(2)
        .reserved_body_bytes(2 * BODY)
        .build()
        .unwrap();
    let api = Api::with_limits(store.clone(), LaneId(1), None, private.clone(), limits).unwrap();
    let router = api.clone().router();
    // The same server at the defaults, for the cost of admission over an unchanged handler.
    let open = Api::with_limits(
        store.clone(),
        LaneId(2),
        None,
        private,
        RequestLimits::default(),
    )
    .unwrap()
    .router();
    let valid = query(BODY);
    let over = query(BODY + 1);
    println!(
        "sample,case,status,iterations,ns_per_request,allocations,reallocations,allocated_bytes,digest"
    );
    let mut peak = IngressCounts::default();
    for sample in 0..3 {
        measure(sample, "valid_default_limits", async || {
            answer(&open, Body::from(valid.clone())).await
        })
        .await;
        measure(sample, "valid", async || {
            answer(&router, Body::from(valid.clone())).await
        })
        .await;
        let served = totals(&store);
        measure(sample, "oversized", async || {
            answer(&router, Body::from(over.clone())).await
        })
        .await;
        // Saturated: both slots held by bodies that have not arrived.
        let mut held = Vec::new();
        for _ in 0..2 {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Chunk>();
            let body = Body::from_stream(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)));
            let r = router.clone();
            held.push((tx, tokio::spawn(async move { answer(&r, body).await })));
        }
        while api.ingress_counts().active < 2 {
            tokio::task::yield_now().await;
        }
        peak = peak.max_by(api.ingress_counts());
        measure(sample, "saturated", async || {
            answer(&router, Body::from(valid.clone())).await
        })
        .await;
        drop(held);
        while api.ingress_counts() != IngressCounts::default() {
            tokio::task::yield_now().await;
        }
        // Cancelled: admitted, then dropped before its body arrives.
        measure(sample, "cancelled", async || {
            let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Chunk>();
            let body = Body::from_stream(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)));
            let r = router.clone();
            let task = tokio::spawn(async move { answer(&r, body).await });
            while api.ingress_counts().active == 0 {
                tokio::task::yield_now().await;
            }
            task.abort();
            assert!(task.await.is_err());
            assert_eq!(api.ingress_counts(), IngressCounts::default());
            (StatusCode::NO_CONTENT, axum::body::Bytes::new())
        })
        .await;
        assert_eq!(
            totals(&store),
            served,
            "a refusal or cancellation reached the store"
        );
    }
    println!(
        "peak_reserved_body_bytes,{},peak_active,{}",
        peak.reserved_body_bytes, peak.active
    );
    assert_eq!(peak.reserved_body_bytes, 2 * BODY);
}

fn totals(store: &Accounted<MemoryStore>) -> [u64; 4] {
    [
        OpClass::Read,
        OpClass::Write,
        OpClass::List,
        OpClass::Delete,
    ]
    .map(|c| store.total(c))
}

trait Peak {
    fn max_by(self, other: Self) -> Self;
}

impl Peak for IngressCounts {
    fn max_by(self, other: Self) -> Self {
        Self {
            active: self.active.max(other.active),
            reserved_body_bytes: self.reserved_body_bytes.max(other.reserved_body_bytes),
            tenants: self.tenants.max(other.tenants),
        }
    }
}
