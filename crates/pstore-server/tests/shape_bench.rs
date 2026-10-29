//! M143: provisional allocation of refused and admitted 2 MiB bodies.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions and fixture construction report measurement failures"
)]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::{Api, Auth, RequestLimits};
use pstore_types::LaneId;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use std::alloc::System;
use std::fmt::Write as _;
use tower::ServiceExt;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// What a refused body may allocate while it is handled, response included.
const REFUSED_BOUND: usize = 64 << 10;

/// Fills `open … close` with `item` until the body is just under 2 MiB.
fn fill(open: &str, item: impl Fn(usize) -> String, sep: &str, close: &str) -> Bytes {
    let max = RequestLimits::default().body_bytes();
    let mut s = String::from(open);
    let mut i = 0;
    loop {
        let next = item(i);
        if s.len() + next.len() + sep.len() + close.len() > max {
            break;
        }
        if i > 0 {
            s.push_str(sep);
        }
        s.push_str(&next);
        i += 1;
    }
    s.push_str(close);
    Bytes::from(s)
}

async fn handle(body: Bytes) -> (StatusCode, usize, usize) {
    let auth =
        Auth::from_vars(|k| (k == "PSTORE_ALLOW_UNAUTHENTICATED").then(|| "1".to_owned())).unwrap();
    let api = Api::with_limits(
        Accounted::new(MemoryStore::new()),
        LaneId(1),
        None,
        auth,
        RequestLimits::default(),
    )
    .unwrap();
    let router = api.clone().router();
    let len = body.len();
    let req = Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "1")
        // One chunk, which collection keeps without copying: the body is not counted.
        .body(Body::from(body))
        .unwrap();
    let region = Region::new(GLOBAL);
    let res = router.oneshot(req).await.unwrap();
    let status = res.status();
    let _ = res.into_body().collect().await.unwrap().to_bytes();
    let stats = region.change();
    (
        status,
        len,
        stats.bytes_allocated + stats.bytes_reallocated.max(0) as usize,
    )
}

#[tokio::test]
#[ignore = "release measurement: cargo test --release -p pstore-server --test shape_bench -- --ignored --nocapture"]
async fn refused_shapes_allocate_little() {
    println!("shape,status,body_bytes,allocated_bytes,allocated_per_body_byte");
    let refused = [
        (
            "wide_array",
            fill(r#"{"deletes":["#, |_| "1".to_owned(), ",", "]}"),
        ),
        (
            "wide_object",
            fill(
                r#"{"documents":[{"id":"a","vector":[1],"attributes":{"#,
                |i| format!("\"k{i}\":1"),
                ",",
                "}}]}",
            ),
        ),
        (
            "long_string",
            fill(
                r#"{"documents":[{"id":""#,
                |_| "a".to_owned(),
                "",
                r#"","vector":[1]}]}"#,
            ),
        ),
        ("deep", fill("", |_| "[".to_owned(), "", "")),
    ];
    for (name, body) in refused {
        let (status, len, allocated) = handle(body).await;
        println!(
            "{name},{},{len},{allocated},{:.4}",
            status.as_u16(),
            allocated as f64 / len as f64
        );
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{name}");
        assert!(allocated <= REFUSED_BOUND, "{name}: {allocated} bytes");
    }
    // Admitted at the defaults: rows of 64 components, as many as fit in 2 MiB.
    let row = |i: usize| {
        let mut r = format!(r#"{{"id":"doc-{i:06}","vector":["#);
        for c in 0..64 {
            let _ = write!(r, "{}0.{c:02}", if c > 0 { "," } else { "" });
        }
        r.push_str("]}");
        r
    };
    let body = fill(r#"{"documents":["#, row, ",", "]}");
    let (status, len, allocated) = handle(body).await;
    assert_eq!(status, StatusCode::OK);
    println!(
        "admitted_write,200,{len},{allocated},{:.2}",
        allocated as f64 / len as f64
    );
}
