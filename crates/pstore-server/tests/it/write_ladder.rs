//! M284: an open-loop write-throughput ladder (lesson 8 of
//! `docs/comparisons/lessons-from-hsearch-opensearch.md`).
//!
//! ⚠️ **Open loop.** Writes arrive at a fixed rate whatever the server does, and each one's
//! latency is counted from when it was due, not from when it was sent: a closed loop of N
//! writers measures latency at the rate the server allows, never the rate it cannot reach.
//! The ladder stops at the first step under 95% acknowledged or over 5 s p99.
//!
//! ⚠️ `provisional`: in process, through the router (no socket), against the in-memory store
//! with every request taking 20 ms, a cloud container's cores. Relative only.
//!
//! The gate scale is `#[ignore]`d; `scripts/write-ladder.sh` runs it in release.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use pstore_blob::{Accounted, Faults, Faulty, MemoryStore, OpClass};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tower::ServiceExt;

type Store = Faulty<MemoryStore>;

/// What a PUT takes: S3's first byte for a small PUT is tens of milliseconds.
const PUT: Duration = Duration::from_millis(20);

fn serve() -> (Arc<Api<Store>>, Accounted<Store>) {
    let faults = Faults {
        latency_min: PUT,
        latency_max: PUT,
        ..Faults::none()
    };
    let store = Accounted::new(Faulty::new(MemoryStore::new(), 1, faults));
    let api = Api::unsecured(store.clone(), LaneId(1)).expect("a fencing backend");
    (api, store)
}

/// One write of `docs` documents to `tenant`: its status.
async fn write(api: Arc<Api<Store>>, tenant: u64, id: u64, docs: usize) -> StatusCode {
    let documents: Vec<_> = (0..docs)
        .map(|d| json!({"id": format!("{id}-{d}"), "vector": [1.0, 0.5, 0.25, 0.125]}))
        .collect();
    let body = json!({"durability": "durable", "documents": documents});
    let req = Request::builder()
        .method("PUT")
        .uri("/v1/indexes/docs/documents")
        .header("content-type", "application/json")
        .header("x-pstore-tenant", tenant.to_string())
        .body(Body::from(body.to_string()))
        .unwrap();
    api.router().oneshot(req).await.unwrap().status()
}

/// One step of the ladder.
#[derive(Debug)]
struct Step {
    rate: u64,
    offered: usize,
    acked: usize,
    p50: Duration,
    p99: Duration,
    puts: u64,
    /// GETs and HEADs.
    reads: u64,
    /// Writes refused at the door, `429`.
    refused: usize,
}

impl Step {
    fn share(&self) -> f64 {
        self.acked as f64 / self.offered.max(1) as f64
    }
    fn holds(&self) -> bool {
        self.share() >= 0.95 && self.p99 <= Duration::from_secs(5)
    }
}

/// Offers `rate` writes a second for `secs`, spread over `tenants`, each of `docs` documents.
async fn step(rate: u64, secs: u64, tenants: u64, docs: usize) -> Step {
    let (api, store) = serve();
    // Every tenant's lane exists before the clock starts: the ladder measures steady writes.
    let warm = (1..=tenants).map(|t| tokio::spawn(write(Arc::clone(&api), t, u64::MAX, 1)));
    for w in warm.collect::<Vec<_>>() {
        assert_eq!(w.await.unwrap(), StatusCode::OK);
    }
    let at = store.total(OpClass::Write);
    let read_at = store.total(OpClass::Read);
    let start = Instant::now();
    let total = rate * secs;
    let mut tasks = Vec::with_capacity(total as usize);
    for i in 0..total {
        let due = start + Duration::from_nanos(i * 1_000_000_000 / rate);
        tokio::time::sleep_until(due).await;
        let api = Arc::clone(&api);
        let tenant = 1 + i % tenants;
        tasks.push(tokio::spawn(async move {
            let status = write(api, tenant, i, docs).await;
            (status, due.elapsed())
        }));
    }
    let mut lat = Vec::with_capacity(tasks.len());
    let (mut acked, mut refused) = (0, 0);
    for t in tasks {
        let (status, l) = t.await.unwrap();
        if status == StatusCode::OK {
            acked += 1;
            lat.push(l);
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            refused += 1;
        }
    }
    lat.sort();
    let pick = |q: f64| {
        lat.get(((lat.len() as f64 * q) as usize).min(lat.len().saturating_sub(1)))
            .copied()
            .unwrap_or(Duration::MAX)
    };
    Step {
        rate,
        offered: total as usize,
        acked,
        p50: pick(0.50),
        p99: pick(0.99),
        puts: store.total(OpClass::Write) - at,
        reads: store.total(OpClass::Read) - read_at,
        refused,
    }
}

/// Climbs `rates` until a step fails, printing each; answers the last rate that held.
async fn ladder(name: &str, rates: &[u64], secs: u64, tenants: u64, docs: usize) -> u64 {
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    println!("# {name}: {tenants} tenant(s), {docs} doc(s) a write, durable, {cores} cores");
    println!(
        "{:>7} {:>8} {:>7} {:>7} {:>9} {:>9} {:>7} {:>7}",
        "rate/s", "offered", "acked", "429", "p50 ms", "p99 ms", "PUTs", "GETs"
    );
    let mut knee = 0;
    for &rate in rates {
        let s = step(rate, secs, tenants, docs).await;
        println!(
            "{:>7} {:>8} {:>6.1}% {:>7} {:>9.1} {:>9.1} {:>7} {:>7}",
            s.rate,
            s.offered,
            100.0 * s.share(),
            s.refused,
            s.p50.as_secs_f64() * 1e3,
            s.p99.as_secs_f64() * 1e3,
            s.puts,
            s.reads
        );
        if !s.holds() {
            break;
        }
        knee = rate;
    }
    let per_core = knee as f64 * docs as f64 / cores as f64;
    println!("# knee: {knee} writes/s, {per_core:.0} docs/s a core (provisional)\n");
    knee
}

/// The suite's form: two low steps of each shape hold, so the harness itself is tested.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_low_rate_holds_on_every_shape() {
    for (tenants, docs) in [(1, 1), (64, 1), (64, 10)] {
        let s = step(50, 1, tenants, docs).await;
        assert!(s.holds(), "{tenants} tenants, {docs} docs: {s:?}");
        assert_eq!(s.acked, 50, "{s:?}");
        // Every durable write lands in some bundle; writes to one tenant can share them.
        assert!(s.puts >= 1 && s.puts <= 50, "{s:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "gate scale: scripts/write-ladder.sh runs it in release"]
async fn the_write_ladder() {
    let rates = [250, 500, 1_000, 2_000, 4_000, 8_000, 16_000];
    let one = ladder("one tenant", &[25, 50, 100, 250, 500, 1_000], 3, 1, 1).await;
    let many = ladder("64 tenants", &rates, 3, 64, 1).await;
    let bulk = ladder("64 tenants, bulks of 10", &rates, 3, 64, 10).await;
    assert!(one > 0 && many > 0 && bulk > 0, "a ladder held no step");
}
