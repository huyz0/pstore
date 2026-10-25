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

//! M119: where one server spends its CPU -- ingest, fold, and each query kind, over one
//! in-memory store with no injected wait, so every millisecond printed is computation.
//!
//! ⚠️ **Not a gate, and never run by `cargo test`.** `scripts/query-bench.sh` runs it in
//! release. The numbers are relative and `provisional` (AGENTS.md): they say whether a change
//! helped on this host, never what a fleet does.
//!
//! Every kind prints a digest of every answer it got, ids and scores, so a change that is
//! faster because it answers differently shows up as a changed digest, not a better number.

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

const SEGMENTS: usize = 24;
const ROWS: usize = 2_000;
const BATCH: usize = 500;
const DIM: usize = 64;
const WARM: usize = 10;
const RUNS: usize = 200;

type Server = Arc<Api<MemoryStore>>;

async fn send(api: &Server, method: &str, uri: &str, body: &Value) -> (u16, Value) {
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
    fn word(&mut self) -> &'static str {
        WORDS[(self.next() % WORDS.len() as u64) as usize]
    }
}

const WORDS: [&str; 24] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo", "sierra", "tango",
    "uniform", "victor", "whiskey", "xray",
];

const LANGS: [&str; 4] = ["en", "fr", "de", "ja"];

fn doc(rng: &mut Rng, i: usize) -> Value {
    let text: Vec<&str> = (0..12).map(|_| rng.word()).collect();
    json!({
        "id": format!("d{i:06}"),
        "vector": rng.vector(),
        "text": text.join(" "),
        "attributes": {
            "n": (rng.next() % 1_000) as i64,
            "lang": LANGS[(rng.next() % 4) as usize],
        },
    })
}

fn percentile(sorted: &[Duration], p: usize) -> f64 {
    let at = (sorted.len() * p / 100).min(sorted.len() - 1);
    sorted[at].as_secs_f64() * 1_000.0
}

/// A query kind: a fresh query from the query seed.
type Make = fn(&mut Rng) -> Value;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a benchmark: run by scripts/query-bench.sh, in release"]
async fn query_bench() {
    let api: Server = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    // Every segment clustered, as at scale: a dense leg reads its centroids, then its lists.
    api.set_index_params_for_test(pstore_index::cluster::Params {
        exact_scan_threshold: 1_000,
        ..pstore_index::cluster::Params::default()
    });

    let mut rng = Rng(0x5eed_0001);
    let mut put = Duration::ZERO;
    let mut fold = Duration::ZERO;
    for k in 0..SEGMENTS {
        for batch in (k * ROWS..(k + 1) * ROWS).step_by(BATCH) {
            let docs: Vec<Value> = (batch..batch + BATCH).map(|i| doc(&mut rng, i)).collect();
            let body = json!({"durability": "durable", "documents": docs});
            let started = Instant::now();
            let (s, b) = send(&api, "PUT", "/v1/indexes/docs/documents", &body).await;
            put += started.elapsed();
            assert_eq!(s, 200, "{b}");
        }
        let started = Instant::now();
        let (s, b) = send(&api, "POST", "/v1/admin/fold", &Value::Null).await;
        fold += started.elapsed();
        assert_eq!(s, 200, "{b}");
    }
    let rows = SEGMENTS * ROWS;
    println!(
        "corpus: {SEGMENTS} segments x {ROWS} rows, dim {DIM}; put {:.0} docs/s, fold {:.1} ms/segment",
        rows as f64 / put.as_secs_f64(),
        fold.as_secs_f64() * 1_000.0 / SEGMENTS as f64,
    );

    let kinds: [(&str, Make); 7] = [
        ("dense", |r| json!({"vector": r.vector(), "top_k": 10})),
        (
            "text",
            |r| json!({"text": format!("{} {}", r.word(), r.word()), "top_k": 10}),
        ),
        (
            "hybrid",
            |r| json!({"vector": r.vector(), "text": r.word(), "top_k": 10}),
        ),
        ("dense+filter", |r| {
            json!({"vector": r.vector(), "top_k": 10,
                   "filters": ["And", [["n", "Lt", 300], ["lang", "Eq", "en"]]]})
        }),
        (
            "text+filter",
            |r| json!({"text": r.word(), "top_k": 10, "filters": ["n", "Gte", 500]}),
        ),
        (
            "dense k=100",
            |r| json!({"vector": r.vector(), "top_k": 100}),
        ),
        (
            "text k=100",
            |r| json!({"text": format!("{} {} {}", r.word(), r.word(), r.word()), "top_k": 100}),
        ),
    ];
    let mut qrng = Rng(0x5eed_0002);
    for (kind, make) in kinds {
        let queries: Vec<Value> = (0..WARM + RUNS).map(|_| make(&mut qrng)).collect();
        let mut took = Vec::with_capacity(RUNS);
        let mut digest = DefaultHasher::new();
        for (n, q) in queries.iter().enumerate() {
            let started = Instant::now();
            let (s, got) = send(&api, "POST", "/v1/indexes/docs/query", q).await;
            let t = started.elapsed();
            assert_eq!(s, 200, "{kind}: {got}");
            let results = got["results"].as_array().unwrap();
            assert!(
                !results.is_empty(),
                "{kind}: an empty answer measures nothing"
            );
            for r in results {
                r["id"].as_str().unwrap().hash(&mut digest);
                r["score"].to_string().hash(&mut digest);
            }
            if n >= WARM {
                took.push(t);
            }
        }
        took.sort();
        println!(
            "{kind:>12}: p50 {:7.3} ms  p95 {:7.3} ms  digest {:016x}",
            percentile(&took, 50),
            percentile(&took, 95),
            digest.finish(),
        );
    }
}
