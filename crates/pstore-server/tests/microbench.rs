//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "a benchmark: assertions report, and its arithmetic is over small counts"
)]

//! M276: per-feature microbenchmarks. One server in process over the in-memory store: server
//! CPU and allocation, with no network and no store latency.
//!
//! Every case runs from fixed seeds, warms up, then repeats, and prints its time's median, p10,
//! p90 and median absolute deviation, its allocations and allocated bytes per operation, and a
//! digest of every answer, so a change that is faster because it answers differently shows up
//! as a changed digest, not a better number.
//!
//! ⚠️ Not a gate, and its numbers are `provisional` (AGENTS.md): relative, on this host only.
//! Run it with nothing else building: `scripts/microbench.sh`.

use axum::body::Body;
use axum::http::Request;
use base64::Engine as _;
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use std::alloc::System;
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

const SEGMENTS: usize = 4;
const ROWS: usize = 2_000;
const BATCH: usize = 500;
const DIM: usize = 64;
const LATE: usize = 8;
const WARM: usize = 5;
const WRITE_RUNS: usize = 40;
const QUERY_RUNS: usize = 100;
/// Writes between two folds, untimed, so buffered rows stay under the tenant's bound (M141).
const FOLD_EVERY: usize = 8;

type Server = Arc<Api<MemoryStore>>;

/// A request whose body is serialized before the clock starts.
fn request(method: &str, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "7")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn send(api: &Server, req: Request<Body>) -> (u16, Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, v)
}

async fn fold(api: &Server) {
    let (s, b) = send(api, request("POST", "/v1/admin/fold", "{}")).await;
    assert_eq!(s, 200, "{b}");
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
    fn vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.unit()).collect()
    }
    fn word(&mut self) -> &'static str {
        WORDS[(self.next() % WORDS.len() as u64) as usize]
    }
    fn text(&mut self, words: usize) -> String {
        (0..words)
            .map(|_| self.word())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

const WORDS: [&str; 24] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo", "sierra", "tango",
    "uniform", "victor", "whiskey", "xray",
];

const LANGS: [&str; 4] = ["en", "fr", "de", "ja"];

fn base64(v: &[f32]) -> String {
    let raw: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(raw)
}

/// A corpus row: a dense vector, a two-vector field, a sparse field, text and attributes.
fn corpus_doc(rng: &mut Rng, i: usize) -> Value {
    let sparse: serde_json::Map<String, Value> = (0..4)
        .map(|_| {
            (
                rng.word().to_owned(),
                json!(f64::from(rng.unit().abs()) + 0.01),
            )
        })
        .collect();
    json!({
        "id": format!("d{i:06}"),
        "vector": rng.vector(DIM),
        "vectors": {"late": [rng.vector(LATE), rng.vector(LATE)]},
        "sparse": sparse,
        "text": rng.text(12),
        "attributes": {"n": (rng.next() % 1_000) as i64, "lang": LANGS[(rng.next() % 4) as usize]},
    })
}

/// A write batch's rows: id, vector, attributes.
fn write_rows(rng: &mut Rng) -> Vec<(String, Vec<f32>, i64, &'static str)> {
    (0..BATCH)
        .map(|i| {
            (
                format!("w{i:06}"),
                rng.vector(DIM),
                (rng.next() % 1_000) as i64,
                LANGS[(rng.next() % 4) as usize],
            )
        })
        .collect()
}

/// The four write bodies, each the same 500 rows but the text-only one.
fn write_bodies() -> Vec<(&'static str, &'static str, String)> {
    let mut rng = Rng(0x5eed_0010);
    let rows = write_rows(&mut rng);
    let arrays: Vec<Value> = rows
        .iter()
        .map(|(id, v, n, l)| json!({"id": id, "vector": v, "attributes": {"n": n, "lang": l}}))
        .collect();
    let b64: Vec<Value> = rows
        .iter()
        .map(|(id, v, n, l)| {
            json!({"id": id, "vector": base64(v), "attributes": {"n": n, "lang": l}})
        })
        .collect();
    let columns = json!({
        "id": rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>(),
        "vector": rows.iter().map(|r| r.1.clone()).collect::<Vec<_>>(),
        "n": rows.iter().map(|r| r.2).collect::<Vec<_>>(),
        "lang": rows.iter().map(|r| r.3).collect::<Vec<_>>(),
    });
    let text: Vec<Value> = (0..BATCH)
        .map(|i| json!({"id": format!("t{i:06}"), "text": rng.text(24)}))
        .collect();
    vec![
        (
            "write rows f32",
            "vecs",
            json!({"durability": "durable", "documents": arrays}).to_string(),
        ),
        (
            "write rows base64",
            "vecs",
            json!({"durability": "durable", "documents": b64}).to_string(),
        ),
        (
            "write columns",
            "vecs",
            json!({"durability": "durable", "upsert_columns": columns}).to_string(),
        ),
        (
            "write text-only",
            "texts",
            json!({"durability": "durable", "documents": text}).to_string(),
        ),
    ]
}

/// A query case: a fresh query from the query seed.
type Make = fn(&mut Rng) -> Value;

fn query_cases() -> [(&'static str, Make); 9] {
    [
        ("bm25", |r| json!({"text": r.text(2), "top_k": 10})),
        ("filter+order", |r| {
            json!({"rank_by": ["n", "asc"], "top_k": 10,
                "filters": ["And", [["n", "Gte", (r.next() % 500) as i64], ["lang", "Eq", "en"]]]})
        }),
        (
            "bm25+filter",
            |r| json!({"text": r.word(), "top_k": 10, "filters": ["lang", "Eq", "fr"]}),
        ),
        (
            "sparse",
            |r| json!({"sparse": {r.word(): 1.0, r.word(): 0.5}, "top_k": 10}),
        ),
        ("maxsim", |r| {
            json!({"multi": {"field": "late", "vectors": [r.vector(LATE), r.vector(LATE)]},
                "top_k": 10})
        }),
        (
            "dense ann",
            |r| json!({"vector": r.vector(DIM), "top_k": 10}),
        ),
        ("multi rrf", |r| {
            json!({"queries": [{"vector": r.vector(DIM), "top_k": 20}, {"text": r.text(2), "top_k": 20}],
                "rerank_by": ["RRF"], "limit": 10})
        }),
        ("compute dist", |r| {
            let q = r.vector(DIM);
            json!({"vector": q.clone(), "top_k": 10,
                "compute_attributes": {"d": ["vector", "VectorDist", q]}})
        }),
        (
            "highlight",
            |r| json!({"text": r.text(2), "top_k": 10, "highlight": {}}),
        ),
    ]
}

/// One case's measurements.
struct Measured {
    took: Vec<Duration>,
    allocs: u64,
    bytes: u64,
    digest: u64,
}

impl Measured {
    fn ms(&self, p: usize) -> f64 {
        let at = (self.took.len() * p / 100).min(self.took.len() - 1);
        self.took[at].as_secs_f64() * 1_000.0
    }
    /// The median absolute deviation, in milliseconds.
    fn mad(&self) -> f64 {
        let median = self.ms(50);
        let mut dev: Vec<f64> = self
            .took
            .iter()
            .map(|t| (t.as_secs_f64() * 1_000.0 - median).abs())
            .collect();
        dev.sort_by(f64::total_cmp);
        dev[dev.len() / 2]
    }
}

/// A digest of an answer: each result's id and score, or a write's counts.
fn digest_answer(h: &mut DefaultHasher, v: &Value) {
    if let Some(results) = v["results"].as_array() {
        assert!(!results.is_empty(), "an empty answer measures nothing: {v}");
        for r in results {
            r["id"].to_string().hash(h);
            r["score"].to_string().hash(h);
            r["dist"].to_string().hash(h);
            r["attributes"].to_string().hash(h);
            r["$highlights"].to_string().hash(h);
        }
    } else {
        for k in [
            "documents_written",
            "documents_deleted",
            "documents_patched",
            "durable",
        ] {
            v[k].to_string().hash(h);
        }
    }
}

fn host() -> String {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|c| {
            c.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, m)| m.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown CPU".to_owned());
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    format!(
        "commit {}, {}, {cpu}, {cores} cores",
        std::env::var("MICROBENCH_COMMIT").unwrap_or_else(|_| "unknown".to_owned()),
        std::env::var("MICROBENCH_RUSTC").unwrap_or_else(|_| "rustc unknown".to_owned()),
    )
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "a benchmark: run by scripts/microbench.sh, in release"]
async fn microbench() {
    let api: Server = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    // Every segment clustered, as at scale: a dense leg reads its centroids, then its lists.
    api.set_index_params_for_test(pstore_index::cluster::Params {
        exact_scan_threshold: 1_000,
        ..pstore_index::cluster::Params::default()
    });
    let mut rng = Rng(0x5eed_0001);
    for k in 0..SEGMENTS {
        for batch in (k * ROWS..(k + 1) * ROWS).step_by(BATCH) {
            let docs: Vec<Value> = (batch..batch + BATCH)
                .map(|i| corpus_doc(&mut rng, i))
                .collect();
            let body = json!({"durability": "durable", "documents": docs}).to_string();
            let (s, b) = send(&api, request("PUT", "/v1/indexes/docs/documents", &body)).await;
            assert_eq!(s, 200, "{b}");
        }
        fold(&api).await;
    }

    let mut out = Vec::new();
    for (name, index, body) in write_bodies() {
        let uri = format!("/v1/indexes/{index}/documents");
        let mut m = Measured {
            took: Vec::with_capacity(WRITE_RUNS),
            allocs: 0,
            bytes: 0,
            digest: 0,
        };
        let mut h = DefaultHasher::new();
        for n in 0..WARM + WRITE_RUNS {
            if n % FOLD_EVERY == 0 {
                fold(&api).await;
            }
            let req = request("PUT", &uri, &body);
            let region = Region::new(GLOBAL);
            let started = Instant::now();
            let (s, got) = send(&api, req).await;
            let t = started.elapsed();
            let stats = region.change();
            assert_eq!(s, 200, "{name}: {got}");
            digest_answer(&mut h, &got);
            if n >= WARM {
                m.took.push(t);
                m.allocs += stats.allocations as u64;
                m.bytes += stats.bytes_allocated as u64;
            }
        }
        m.digest = h.finish();
        out.push((name, m));
    }
    fold(&api).await;

    let mut qrng = Rng(0x5eed_0002);
    for (name, make) in query_cases() {
        let queries: Vec<String> = (0..WARM + QUERY_RUNS)
            .map(|_| make(&mut qrng).to_string())
            .collect();
        let mut m = Measured {
            took: Vec::with_capacity(QUERY_RUNS),
            allocs: 0,
            bytes: 0,
            digest: 0,
        };
        let mut h = DefaultHasher::new();
        for (n, q) in queries.iter().enumerate() {
            let req = request("POST", "/v1/indexes/docs/query", q);
            let region = Region::new(GLOBAL);
            let started = Instant::now();
            let (s, got) = send(&api, req).await;
            let t = started.elapsed();
            let stats = region.change();
            assert_eq!(s, 200, "{name}: {got}");
            digest_answer(&mut h, &got);
            if n >= WARM {
                m.took.push(t);
                m.allocs += stats.allocations as u64;
                m.bytes += stats.bytes_allocated as u64;
            }
        }
        m.digest = h.finish();
        out.push((name, m));
    }

    let host = host();
    println!("microbench (provisional): {host}");
    println!(
        "corpus: {SEGMENTS} segments x {ROWS} rows, dim {DIM}, late 2x{LATE}; writes {BATCH} rows; \
         warmups {WARM}, runs {WRITE_RUNS} a write, {QUERY_RUNS} a query"
    );
    let mut csv = String::from(
        "case,runs,median_ms,p10_ms,p90_ms,mad_ms,allocs_per_op,bytes_per_op,digest\n",
    );
    println!(
        "{:>18} {:>9} {:>9} {:>9} {:>8} {:>11} {:>12}  digest",
        "case", "median ms", "p10 ms", "p90 ms", "MAD ms", "allocs/op", "bytes/op"
    );
    for (name, mut m) in out {
        m.took.sort();
        let runs = m.took.len() as u64;
        let (allocs, bytes) = (m.allocs / runs, m.bytes / runs);
        println!(
            "{name:>18} {:9.3} {:9.3} {:9.3} {:8.3} {allocs:11} {bytes:12}  {:016x}",
            m.ms(50),
            m.ms(10),
            m.ms(90),
            m.mad(),
            m.digest
        );
        writeln!(
            csv,
            "{name},{runs},{:.4},{:.4},{:.4},{:.4},{allocs},{bytes},{:016x}",
            m.ms(50),
            m.ms(10),
            m.ms(90),
            m.mad(),
            m.digest
        )
        .unwrap();
    }
    if let Ok(path) = std::env::var("MICROBENCH_CSV") {
        std::fs::write(&path, format!("# {host}\n{csv}")).unwrap();
        println!("wrote {path}");
    }
}
