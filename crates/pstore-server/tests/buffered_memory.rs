//! M213: the heap buffered rows hold, against what the tenant is charged for them.
//!
//! ⚠️ One test in this binary, on purpose: the counting allocator is global, so a second test
//! running beside it would allocate inside its region.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
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

/// One row of each shape: its id is `i`, the rest is the shape's.
fn row(shape: &str, i: usize) -> Value {
    let id = format!("doc-{i:06}");
    match shape {
        "tiny" => json!({"id": id, "vector": [0.5, 0.25]}),
        "attrs" => json!({"id": id, "vector": [0.5, 0.25],
            "attributes": {"title": format!("title of document {i}"), "n": i, "ok": true}}),
        "wide" => json!({"id": id, "vector": vec![0.5_f32; 128]}),
        "huge" => json!({"id": id, "vector": vec![0.5_f32; 768]}),
        "many" => {
            let attrs: serde_json::Map<String, Value> =
                (0..20).map(|k| (format!("a{k:02}"), json!(k))).collect();
            json!({"id": id, "vector": [0.5, 0.25], "attributes": attrs})
        }
        "arrays" => json!({"id": id, "vector": [0.5, 0.25],
            "attributes": {"tags": ["red", "green", "blue", "a much longer tag value"],
                           "ns": [1, 2, 3, 4, 5, 6, 7, 8]}}),
        "sparse" => json!({"id": id, "vector": [0.5, 0.25],
            "sparse": {"apple": 0.5, "banana": 0.25, "cherry": 1.0}}),
        d if d.starts_with('d') => {
            json!({"id": id, "vector": vec![0.5_f32; d[1..].parse::<usize>().unwrap()]})
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn buffered_rows_are_charged_at_least_the_heap_they_hold() {
    println!("shape,rows,charged_bytes,heap_net_bytes,charged_per_heap");
    for (shape, writes, per) in [
        ("tiny", 20, 100),
        ("d64", 5, 100),
        ("d1024", 5, 100),
        ("attrs", 20, 100),
        ("wide", 20, 100),
        ("huge", 5, 100),
        ("many", 5, 100),
        ("arrays", 20, 100),
        ("sparse", 20, 100),
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
        let warm = json!({"durability": "durable", "documents": [row(shape, 999_999)]});
        put(&api, &warm).await;
        put(
            &api,
            &json!({"durability": "batched", "documents": [row(shape, 999_998)]}),
        )
        .await;
        let engine = api.hold_engine_for_test(TenantId(1)).await;
        let before = engine.buffered();
        let region = Region::new(GLOBAL);
        for w in 0..writes {
            let docs: Vec<Value> = (0..per).map(|i| row(shape, w * per + i)).collect();
            put(&api, &json!({"durability": "batched", "documents": docs})).await;
        }
        let stats = region.change();
        let after = engine.buffered();
        assert_eq!(
            after.unfolded, before.unfolded,
            "{shape}: no flush inside the region"
        );
        let charged = after.pending - before.pending;
        let net = stats.bytes_allocated as i64 - stats.bytes_deallocated as i64;
        let ratio = charged as f64 / net as f64;
        println!("{shape},{},{charged},{net},{ratio:.2}", writes * per);
        // ⚠️ Allocated less deallocated: `stats_alloc` adds a reallocation's growth to
        // `bytes_allocated` as well as to `bytes_reallocated`, so summing both counts it twice.
        assert!(
            (1.0..=2.5).contains(&ratio),
            "{shape}: charged {charged} for {net} bytes of heap ({ratio:.2}x)"
        );
    }
}
