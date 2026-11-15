#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "a measurement harness: a failure here is a bug in the harness, and panicking \
              names it immediately"
)]

//! What M264's `meta.performance` adds to a query, against what a warm query costs.
//!
//! M264 adds, per query: two `Instant::now()` and two `elapsed()`, two reads of the tenant's
//! cache counters (two relaxed loads each), one relaxed add per cacheable read (and one per
//! missed range, none when warm), and five more fields in the response. This times each of
//! those over a million repetitions, bounds the cacheable reads of one query by the store
//! requests the same query makes cold (each cold cache call reaches the store at least once),
//! and prints the sum beside the median of `QUERIES` warm queries through the router.
//!
//! ⚠️ **Every time it prints is `provisional`**: an in-memory store, one container. The
//! ratio is the number; the milliseconds are not.
//!
//!   cargo run --release -p pstore-server --example perf_overhead

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_cache::CacheCore;
use pstore_server::{Api, Performance};
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tower::ServiceExt;

const ROWS: u32 = 20_000;
const QUERIES: usize = 2_000;
const REPS: u32 = 1_000_000;

type A = Arc<Api<MemoryStore>>;

async fn send(api: &A, uri: &str, body: &Value, method: &str) -> Value {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(r).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn cached(store: &Accounted<MemoryStore>) -> A {
    Api::with_cache_unsecured(
        store.clone(),
        LaneId(1),
        Arc::new(CacheCore::memory(256 << 20)),
    )
    .unwrap()
}

/// Nanoseconds per repetition of `f`.
fn per(f: impl Fn()) -> f64 {
    let t = Instant::now();
    for _ in 0..REPS {
        f();
    }
    t.elapsed().as_nanos() as f64 / f64::from(REPS)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let store = Accounted::new(MemoryStore::new());
    let writer = cached(&store);
    for chunk in (0..ROWS).collect::<Vec<_>>().chunks(5_000) {
        let documents: Vec<Value> = chunk
            .iter()
            .map(|&i| {
                let x = f64::from(i);
                json!({"id": format!("d{i:05}"), "vector": [x.sin(), x.cos(), (x / 7.0).sin()],
                       "attributes": {"n": i}})
            })
            .collect();
        let body = json!({"durability": "durable", "documents": documents});
        send(&writer, "/v1/indexes/docs/documents", &body, "PUT").await;
    }
    send(&writer, "/v1/admin/fold", &json!({}), "POST").await;

    let q = json!({"vector": [0.3, 0.9, 0.1], "top_k": 10, "filters": ["n", "Gte", 100]});
    let api = cached(&store);
    let cold = send(&api, "/v1/indexes/docs/query", &q, "POST").await;
    let calls = cold["meta"]["cost"]["blob_reads"].as_u64().unwrap();
    let mut times: Vec<Duration> = Vec::with_capacity(QUERIES);
    let mut last = Value::Null;
    for _ in 0..QUERIES {
        let t = Instant::now();
        last = send(&api, "/v1/indexes/docs/query", &q, "POST").await;
        times.push(t.elapsed());
    }
    times.sort_unstable();
    let median_ns = times[QUERIES / 2].as_nanos() as f64;
    assert_eq!(
        last["meta"]["performance"]["cache_hit_ratio"], 1.0,
        "{last}"
    );

    let counter = AtomicU64::new(0);
    let clock = per(|| {
        black_box(Instant::now());
    });
    let elapsed = per(|| {
        let t = black_box(Instant::now());
        black_box(t.elapsed());
    }) - clock;
    let add = per(|| {
        counter.fetch_add(black_box(64), Ordering::Relaxed);
    });
    let load = per(|| {
        black_box(counter.load(Ordering::Relaxed));
    });
    let perf = Performance::of(
        Instant::now(),
        Duration::from_micros(250),
        pstore_cache::Traffic {
            hit_bytes: 123_456,
            miss_bytes: 0,
        },
        pstore_cache::Traffic::default(),
    );
    let render = per(|| {
        black_box(serde_json::to_vec(black_box(&perf)).unwrap());
    });
    // Two clocks, two elapsed, four loads, one add per cache call, the five fields.
    let added = 2.0 * clock + 2.0 * elapsed + 4.0 * load + calls as f64 * add + render;
    println!("rows {ROWS}, warm queries {QUERIES} (provisional: in-memory store)");
    println!("median warm query      {:>10.1} us", median_ns / 1e3);
    println!("cache calls, bound     {calls:>10}");
    println!(
        "clock {clock:.1} ns, elapsed {elapsed:.1} ns, load {load:.1} ns, add {add:.1} ns, \
         render {render:.1} ns"
    );
    println!("M264 adds per query    {:>10.1} ns", added);
    println!(
        "share of the median    {:>10.3} %",
        added / median_ns * 100.0
    );
}
