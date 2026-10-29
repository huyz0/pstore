//! M141: provisional heap held by buffered rows, against their `footprint()` estimate.
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
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::{Api, Auth, RequestLimits};
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use std::alloc::System;
use std::sync::Arc;
use tower::ServiceExt;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

const ROWS: usize = 100;

async fn put(api: &Arc<Api<MemoryStore>>, body: &Value) {
    let req = Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "1")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = api.clone().router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
}

fn rows(dims: usize, from: usize, attrs: bool) -> Value {
    let docs: Vec<Value> = (from..from + ROWS)
        .map(|i| {
            let mut d = json!({"id": format!("doc-{i:06}"), "vector": vec![0.5_f32; dims]});
            if attrs {
                d["attributes"] = json!({"title": format!("title of document {i}"), "n": i});
            }
            d
        })
        .collect();
    json!({"durability": "batched", "documents": docs})
}

#[tokio::test]
#[ignore = "release measurement: cargo test --release -p pstore-server --test buffered_bench -- --ignored --nocapture"]
async fn buffered_footprint_against_heap() {
    println!("dims,attributes,rows,footprint_bytes,heap_net_bytes,heap_per_footprint");
    // Few enough writes that no case reaches the 8 MiB flush bound: every row stays pending.
    for (dims, attrs, writes) in [
        (2, false, 40),
        (2, true, 40),
        (128, false, 40),
        (128, true, 40),
        (768, false, 10),
    ] {
        let auth =
            Auth::from_vars(|k| (k == "PSTORE_ALLOW_UNAUTHENTICATED").then(|| "1".to_owned()))
                .unwrap();
        let api = Api::with_limits(
            Accounted::new(MemoryStore::new()),
            LaneId(1),
            None,
            auth,
            RequestLimits::default(),
        )
        .unwrap();
        // Register the lane and warm the request path, so the region holds rows, not setup.
        put(&api, &json!({"durability": "durable", "documents": [{"id": "warm", "vector": vec![0.5_f32; dims]}]})).await;
        let engine = api.hold_engine_for_test(TenantId(1)).await;
        let before = engine.buffered();
        let region = Region::new(GLOBAL);
        for w in 0..writes {
            put(&api, &rows(dims, w * ROWS, attrs)).await;
        }
        let stats = region.change();
        let after = engine.buffered();
        assert_eq!(
            after.unfolded, before.unfolded,
            "no flush inside the region"
        );
        let footprint = after.pending - before.pending;
        let net = stats.bytes_allocated as i64 + stats.bytes_reallocated as i64
            - stats.bytes_deallocated as i64;
        assert!(footprint > 0);
        println!(
            "{dims},{attrs},{},{footprint},{net},{:.2}",
            writes * ROWS,
            net as f64 / footprint as f64
        );
    }
}
