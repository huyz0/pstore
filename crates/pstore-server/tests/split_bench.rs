//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::print_stdout,
    reason = "a benchmark: assertions and printed numbers are its output"
)]

//! M57: what a split buys, measured provisionally -- one unpeered server against a coordinator
//! of three, in one process, over one in-memory store, on loopback.
//!
//! ⚠️ **Not a gate, and never run by `cargo test`.** `scripts/split-bench.sh` runs it in release.
//! The numbers are relative and `provisional` (AGENTS.md): one host shares its cores among the
//! three servers, so this bounds the split's overhead and shows whether it serialises
//! anything. It says nothing about separate machines, which M0b's fleet must measure.

use axum::body::Body;
use axum::http::Request;
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition,
    PutOutcome,
};
use pstore_server::{Api, PeerConfig};
use pstore_types::{CasTag, LaneId};
use serde_json::{Value, json};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tower::ServiceExt;

const SEGMENTS: usize = 48;
const ROWS: usize = 2_000;
const DIM: usize = 64;
const WARM: usize = 5;
const RUNS: usize = 100;
const WAIT_MS: u64 = 20;

/// The shared store, as one server sees it: waiting `delay_ms` before every read.
#[derive(Debug, Clone)]
struct Delayed {
    inner: MemoryStore,
    delay_ms: Arc<AtomicU64>,
}

impl Delayed {
    async fn wait(&self) {
        let ms = self.delay_ms.load(Ordering::Relaxed);
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for Delayed {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.wait().await;
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.wait().await;
        self.inner.get_range(key, range).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        _: Class,
    ) -> Result<Bytes, BlobError> {
        self.wait().await;
        self.inner.get_range(key, range).await
    }
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        self.wait().await;
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[Range<u64>],
        _: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        self.wait().await;
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.wait().await;
        self.inner.get_suffix(key, n).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, _: Class) -> Result<Bytes, BlobError> {
        self.wait().await;
        self.inner.get_suffix(key, n).await
    }
    async fn get_immutable(&self, key: &Key, _: Class) -> Result<Bytes, BlobError> {
        self.wait().await;
        self.inner.get(key).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.wait().await;
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.wait().await;
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.wait().await;
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

struct Server {
    api: Arc<Api<Delayed>>,
    store: Delayed,
    url: String,
}

async fn start(store: &MemoryStore, lane: u64) -> (Server, tokio::net::TcpListener) {
    let view = Delayed {
        inner: store.clone(),
        delay_ms: Arc::default(),
    };
    let api = Api::new(Accounted::new(view.clone()), LaneId(lane)).unwrap();
    // Every segment clustered, as at scale: a dense leg reads its centroids, then its lists.
    api.set_index_params_for_test(pstore_index::cluster::Params {
        exact_scan_threshold: 1_000,
        ..pstore_index::cluster::Params::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        Server {
            api,
            store: view,
            url,
        },
        listener,
    )
}

async fn send(api: &Arc<Api<Delayed>>, method: &str, uri: &str, body: Value) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, v)
}

async fn metric(api: &Arc<Api<Delayed>>, name: &str) -> u64 {
    let req = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let text =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{name} ")))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// SplitMix64: the corpus and the queries from fixed seeds, so every run measures the same.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }
    fn vector(&mut self) -> Vec<f32> {
        (0..DIM).map(|_| self.unit()).collect()
    }
}

/// A query kind: a fresh query from the query seed.
type Make = Box<dyn Fn(&mut Rng) -> Value>;

const WORDS: [&str; 12] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima",
];

fn doc(rng: &mut Rng, i: usize) -> Value {
    let text: Vec<&str> = (0..6)
        .map(|_| WORDS[(rng.next() % WORDS.len() as u64) as usize])
        .collect();
    json!({
        "id": format!("d{i:06}"),
        "vector": rng.vector(),
        "text": text.join(" "),
    })
}

fn percentile(sorted: &[Duration], p: usize) -> f64 {
    let at = (sorted.len() * p / 100).min(sorted.len() - 1);
    sorted[at].as_secs_f64() * 1_000.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a benchmark: run by scripts/split-bench.sh, in release"]
async fn split_bench() {
    let store = MemoryStore::new();
    let mut servers = Vec::new();
    let mut listeners = Vec::new();
    for lane in 1..=3 {
        let (s, l) = start(&store, lane).await;
        servers.push(s);
        listeners.push(l);
    }
    let urls: Vec<String> = servers.iter().map(|s| s.url.clone()).collect();
    for (me, s) in servers.iter().enumerate() {
        s.api
            .set_peers(Some(PeerConfig {
                servers: urls.clone(),
                me,
                timeout: Duration::from_secs(30),
            }))
            .unwrap();
    }
    for (s, l) in servers.iter().zip(listeners) {
        tokio::spawn(pstore_server::serve(
            Arc::clone(&s.api),
            l,
            std::future::pending(),
        ));
    }
    let (alone, _) = start(&store, 9).await;

    // The corpus: one fold per segment.
    let built = Instant::now();
    let mut rng = Rng(0x5eed_0001);
    for k in 0..SEGMENTS {
        // In batches under the request body limit, then one fold: one segment.
        for batch in (k * ROWS..(k + 1) * ROWS).step_by(500) {
            let docs: Vec<Value> = (batch..batch + 500).map(|i| doc(&mut rng, i)).collect();
            let (s, b) = send(
                &servers[0].api,
                "PUT",
                "/v1/indexes/docs/documents",
                json!({"durability": "durable", "documents": docs}),
            )
            .await;
            assert_eq!(s, 200, "{b}");
        }
        let (s, b) = send(&servers[0].api, "POST", "/v1/admin/fold", Value::Null).await;
        assert_eq!(s, 200, "{b}");
    }
    let segs: Vec<String> = store
        .list_unrestricted(&Key::new(String::new()))
        .await
        .unwrap()
        .into_iter()
        .map(|k| k.as_str().to_owned())
        .filter(|k| k.ends_with(".seg"))
        .collect();
    let mut held = [0usize; 3];
    for s in &segs {
        held[pstore_engine::assign(s, &urls)] += 1;
    }
    assert_eq!(segs.len(), SEGMENTS, "the layout this measures");
    assert!(
        held.iter().all(|n| *n > 0),
        "a server holds nothing: {held:?}"
    );
    println!(
        "layout: {} segments x {ROWS} rows, dim {DIM}, held {held:?}, built in {:.1} s",
        segs.len(),
        built.elapsed().as_secs_f64()
    );

    let mut qrng = Rng(0x5eed_0002);
    let kinds: Vec<(&str, Make)> = vec![
        (
            "dense",
            Box::new(|r: &mut Rng| json!({"vector": r.vector(), "top_k": 10})),
        ),
        (
            "text",
            Box::new(|r: &mut Rng| {
                let a = WORDS[(r.next() % 12) as usize];
                let b = WORDS[(r.next() % 12) as usize];
                json!({"text": format!("{a} {b}"), "top_k": 10})
            }),
        ),
        (
            "hybrid",
            Box::new(|r: &mut Rng| {
                let a = WORDS[(r.next() % 12) as usize];
                json!({"vector": r.vector(), "text": a, "top_k": 10})
            }),
        ),
    ];
    let all: Vec<&Delayed> = servers
        .iter()
        .map(|s| &s.store)
        .chain(std::iter::once(&alone.store))
        .collect();
    for (backend, wait) in [("cpu", 0u64), ("wait", WAIT_MS)] {
        for s in &all {
            s.delay_ms.store(wait, Ordering::Relaxed);
        }
        for (kind, make) in &kinds {
            let queries: Vec<Value> = (0..WARM + RUNS).map(|_| make(&mut qrng)).collect();
            let mut p50 = [0.0f64; 2];
            for (l, (layout, api)) in [("unsplit", &alone.api), ("split", &servers[0].api)]
                .into_iter()
                .enumerate()
            {
                let sent = metric(api, "pstore_peer_parts_sent").await;
                let mut took = Vec::with_capacity(RUNS);
                for (n, q) in queries.iter().enumerate() {
                    let started = Instant::now();
                    let (s, got) = send(api, "POST", "/v1/indexes/docs/query", q.clone()).await;
                    let t = started.elapsed();
                    assert_eq!(s, 200, "{got}");
                    if l == 1 {
                        // A fast wrong answer never reaches the ledger.
                        let (_, want) =
                            send(&alone.api, "POST", "/v1/indexes/docs/query", q.clone()).await;
                        assert_eq!(
                            got["results"], want["results"],
                            "{backend} {kind}: the split answer differs"
                        );
                    }
                    if n >= WARM {
                        took.push(t);
                    }
                }
                took.sort();
                let parts = (metric(api, "pstore_peer_parts_sent").await - sent) as f64
                    / (WARM + RUNS) as f64;
                p50[l] = percentile(&took, 50);
                println!(
                    "{backend:>4} {kind:>6} {layout:>7}: p50 {:8.2} ms  p95 {:8.2} ms  parts/query {parts:.1}",
                    p50[l],
                    percentile(&took, 95),
                );
            }
            println!(
                "{backend:>4} {kind:>6}   ratio: split/unsplit p50 = {:.2}",
                p50[1] / p50[0]
            );
        }
    }
}
