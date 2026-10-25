//! Provisional instrumented HTTP admission costs, outside ordinary test runs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions and fixture construction report measurement failures"
)]
mod support;
use axum::{
    Router,
    body::Body,
    http::{HeaderValue, Request, StatusCode},
};
use http_body_util::BodyExt;
use jsonwebtoken::Algorithm;
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_server::Api;
use pstore_types::LaneId;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use std::alloc::System;
use std::hint::black_box;
use std::time::Instant;
use support::auth::{KEY, OTHER_KEY, auth, claims, config, sign, token};
use tower::ServiceExt;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

async fn request(router: &Router, bearer: &HeaderValue) -> (StatusCode, axum::body::Bytes) {
    let req = Request::builder()
        .uri("/v1/admin/duties")
        .header("authorization", bearer.clone())
        .header("x-pstore-tenant", "9")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes)
}

fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}

async fn measure(
    sample: usize,
    name: &str,
    router: &Router,
    bearer: &HeaderValue,
    expected: StatusCode,
    body: &[u8],
) {
    for _ in 0..1000 {
        let (status, actual) = request(router, bearer).await;
        assert_eq!(status, expected);
        assert_eq!(actual.as_ref(), body);
    }
    let n = 5000;
    let start = Instant::now();
    let region = Region::new(GLOBAL);
    let mut outcome = 0_u64;
    for _ in 0..n {
        let (status, actual) = request(black_box(router), black_box(bearer)).await;
        assert_eq!(status, expected);
        assert_eq!(actual.as_ref(), body);
        outcome = outcome.wrapping_add(black_box(digest(&actual)));
    }
    let elapsed = start.elapsed().as_nanos();
    let stats = region.change();
    assert_eq!(outcome, digest(body).wrapping_mul(n));
    println!(
        "{sample},{name},{n},{:.2},{:.2},{:.2},{:.2},{outcome}",
        elapsed as f64 / n as f64,
        stats.allocations as f64 / n as f64,
        stats.reallocations as f64 / n as f64,
        stats.bytes_allocated as f64 / n as f64
    );
}

/// Run this binary alone so global allocator counters have no competing tests.
#[tokio::test]
#[ignore = "provisional CPU/allocation campaign; run explicitly with --release --ignored --nocapture"]
async fn admission_cpu_allocations_and_response_digests() {
    let store = Accounted::new(MemoryStore::new());
    let private = Api::unsecured(store.clone(), LaneId(1)).unwrap().router();
    let secured = Api::with_auth(
        store.clone(),
        LaneId(2),
        auth(&config(&[("current", &KEY)], "current")),
    )
    .unwrap()
    .router();
    let valid = HeaderValue::from_str(&format!("Bearer {}", token("9", &["admin"]))).unwrap();
    let invalid = HeaderValue::from_str(&format!(
        "Bearer {}",
        sign(
            &claims("9", &["admin"]),
            Some("current"),
            Algorithm::HS256,
            &OTHER_KEY
        )
    ))
    .unwrap();
    let (status, body) = request(&private, &valid).await;
    assert_eq!(status, StatusCode::OK);
    let (status, authenticated) = request(&secured, &valid).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(authenticated, body);
    let (status, rejected) = request(&secured, &invalid).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let error: serde_json::Value = serde_json::from_slice(&rejected).unwrap();
    assert_eq!(error["error"]["code"], "unauthenticated");
    println!(
        "sample,case,iterations,ns_per_request,allocations,reallocations,allocated_bytes,digest"
    );
    for sample in 0..5 {
        // Reverse order on odd samples to expose simple order/drift effects.
        let cases = if sample % 2 == 0 {
            ["private", "valid_jwt", "invalid_signature"]
        } else {
            ["invalid_signature", "valid_jwt", "private"]
        };
        for case in cases {
            let (router, header, status, expected) = match case {
                "private" => (&private, &valid, StatusCode::OK, body.as_ref()),
                "valid_jwt" => (&secured, &valid, StatusCode::OK, body.as_ref()),
                _ => (
                    &secured,
                    &invalid,
                    StatusCode::UNAUTHORIZED,
                    rejected.as_ref(),
                ),
            };
            measure(sample, case, router, header, status, expected).await;
        }
    }
    for class in [
        OpClass::Read,
        OpClass::Write,
        OpClass::List,
        OpClass::Delete,
    ] {
        assert_eq!(store.total(class), 0, "admission issued {class:?}");
    }
}
