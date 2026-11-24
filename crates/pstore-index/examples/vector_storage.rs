#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    reason = "a measurement harness: a failure here is a bug in the harness, and panicking \
              names it immediately"
)]

//! M289's bench: what storing a segment's dense vectors as f16 or i8 saves and costs.
//!
//! ⚠️ Runs **outside `cargo test`**, for `recall.rs`'s reason. Numbers are `provisional`: a
//! cloud container, synthetic corpora, release build, one segment in memory.
//!
//!   cargo run --release -p pstore-index --example vector_storage
//!
//! Per corpus and mode: the `Vectors` section's bytes; recall@10 against exact f32 truth of
//! the exact search, of ANN with `rerank: exact` (the one ANN rung that reads the stored
//! vectors) and of the default ANN (which reads only codes built from f32, so must not move);
//! the exact top-10's overlap with f32's exact top-10; the largest score error of a row the
//! exact search returns; one exact scan's time (fetch, decode, score); and the build's time.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_format::narrow::{Elem, Vectors};
use pstore_format::{BlockOptions, Document, Section, VectorField};
use pstore_index::cluster::Params;
use pstore_index::vec_index::{self, Query, Rerank, VecIndex};
use std::collections::BTreeMap;
use std::time::Instant;

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    }
    fn normal(&mut self) -> f32 {
        (0..6).map(|_| self.unit()).sum()
    }
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in &mut v {
            *x /= n;
        }
    }
    v
}

/// A corpus as the engine stores it: `stored` is what the segment holds (normalized under
/// cosine; with euclidean's `-|x|^2 / 2` appended), `queries` as the engine scores them.
struct Corpus {
    name: String,
    stored: Vec<Vec<f32>>,
    queries: Vec<Vec<f32>>,
    tail: u8,
}

const DIM: usize = 384;

fn clustered(n: usize, normal: bool, seed: u64) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    let mut rng = Rng(seed);
    let centres: Vec<Vec<f32>> = (0..40)
        .map(|_| (0..DIM).map(|_| rng.normal() * 1.5).collect())
        .collect();
    let mut make = |i: usize| {
        let v: Vec<f32> = centres[i % 40].iter().map(|x| x + rng.normal()).collect();
        if normal { normalize(v) } else { v }
    };
    let vectors = (0..n).map(&mut make).collect();
    let queries = (0..100).map(&mut make).collect();
    (vectors, queries)
}

fn uniform(n: usize, seed: u64) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    let mut rng = Rng(seed);
    let mut make = || normalize((0..DIM).map(|_| rng.normal()).collect());
    let vectors = (0..n).map(|_| make()).collect();
    let queries = (0..100).map(|_| make()).collect();
    (vectors, queries)
}

fn corpora() -> Vec<Corpus> {
    let (cv, cq) = clustered(20_000, true, 1);
    let (uv, uq) = uniform(10_000, 2);
    let (ev, eq) = clustered(20_000, false, 3);
    vec![
        Corpus {
            name: "20,000 x 384d clustered, cosine".to_owned(),
            stored: cv,
            queries: cq,
            tail: 0,
        },
        Corpus {
            name: "10,000 x 384d uniform, cosine".to_owned(),
            stored: uv,
            queries: uq,
            tail: 0,
        },
        // M9d's transform: rows `x ++ [-|x|^2/2]`, queries `q ++ [1]`.
        Corpus {
            name: "20,000 x 384d clustered unnormalized, euclidean".to_owned(),
            stored: ev
                .into_iter()
                .map(|v| {
                    let n2: f32 = v.iter().map(|x| x * x).sum();
                    v.into_iter().chain([-n2 / 2.0]).collect()
                })
                .collect(),
            queries: eq
                .into_iter()
                .map(|q| q.into_iter().chain([1.0]).collect())
                .collect(),
            tail: 1,
        },
    ]
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Exact f32 top-10 by dot over `stored`, as document indexes.
fn truth(c: &Corpus) -> Vec<Vec<usize>> {
    c.queries
        .iter()
        .map(|q| {
            let mut s: Vec<(usize, f32)> = c
                .stored
                .iter()
                .enumerate()
                .map(|(i, v)| (i, dot(v, q)))
                .collect();
            s.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            s.into_iter().take(10).map(|(i, _)| i).collect()
        })
        .collect()
}

fn overlap(a: &[usize], b: &[usize]) -> f64 {
    a.iter().filter(|x| b.contains(x)).count() as f64 / b.len().max(1) as f64
}

struct Row {
    bytes: usize,
    exact: f64,
    rerank: f64,
    ann: f64,
    overlap: f64,
    err: f32,
    scan_ms: f64,
    build_ms: f64,
}

async fn measure(
    c: &Corpus,
    truth: &[Vec<usize>],
    f32_exact: Option<&[Vec<usize>]>,
    elem: Elem,
) -> (Row, Vec<Vec<usize>>) {
    let docs: Vec<Document> = c
        .stored
        .iter()
        .enumerate()
        .map(|(i, v)| Document {
            id: format!("d{i:06}"),
            vectors: BTreeMap::from([(
                pstore_format::DEFAULT_FIELD.into(),
                VectorField::dense(v.clone()),
            )]),
            attrs: BTreeMap::new(),
        })
        .collect();
    let params = Params {
        blocks: BlockOptions {
            vectors: Vectors { elem, tail: c.tail },
            ..BlockOptions::default()
        },
        // Clustered at this scale: the default threshold (25,000) would scan both corpora.
        exact_scan_threshold: 1_000,
        ..Params::default()
    };
    let t = Instant::now();
    let built = vec_index::build(&docs, params);
    let build_ms = t.elapsed().as_secs_f64() * 1e3;
    let store = MemoryStore::new();
    let (seg, cen) = (Key::new("s.seg"), Key::new("s.cen"));
    store.put(&seg, built.segment.clone()).await.unwrap();
    store
        .put(
            &cen,
            bytes::Bytes::from(built.centroids.as_ref().unwrap().encode()),
        )
        .await
        .unwrap();
    let idx = VecIndex::open(&store, &seg, &cen, c.stored[0].len())
        .await
        .unwrap();
    let span = idx.segment().section(Section::Vectors).unwrap();
    let bytes = (span.end - span.start) as usize;
    let doc = |row: usize| built.order[row];
    let (mut exact, mut rerank, mut ann, mut over, mut err) = (0.0, 0.0, 0.0, 0.0, 0f32);
    let mut scan = 0.0;
    let mut answered = Vec::new();
    for (qi, q) in c.queries.iter().enumerate() {
        let t = Instant::now();
        let got = idx
            .search(
                &store,
                &seg,
                q,
                Query {
                    exact: true,
                    ..Query::default()
                },
            )
            .await
            .unwrap();
        scan += t.elapsed().as_secs_f64() * 1e3;
        let ids: Vec<usize> = got.iter().map(|(r, _)| doc(*r)).collect();
        for (r, s) in &got {
            err = err.max((s - dot(&c.stored[doc(*r)], q)).abs());
        }
        exact += overlap(&ids, &truth[qi]);
        over += f32_exact.map_or(1.0, |f| overlap(&ids, &f[qi]));
        answered.push(ids);
        for (rr, acc) in [(Rerank::Exact, &mut rerank), (Rerank::Fast, &mut ann)] {
            let got = idx
                .search(
                    &store,
                    &seg,
                    q,
                    Query {
                        rerank: rr,
                        ..Query::default()
                    },
                )
                .await
                .unwrap();
            let ids: Vec<usize> = got.iter().map(|(r, _)| doc(*r)).collect();
            *acc += overlap(&ids, &truth[qi]);
        }
    }
    let n = c.queries.len() as f64;
    (
        Row {
            bytes,
            exact: exact / n,
            rerank: rerank / n,
            ann: ann / n,
            overlap: over / n,
            err,
            scan_ms: scan / n,
            build_ms,
        },
        answered,
    )
}

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        for c in corpora() {
            let truth = truth(&c);
            println!("# {} (provisional), k=10, default p and oversample", c.name);
            println!(
                "mode  vectors_bytes  ratio  r@10_exact  r@10_rerank_exact  r@10_default  \
                 exact_overlap  max_score_err  exact_scan_ms  build_ms"
            );
            let (base, f32_exact) = measure(&c, &truth, None, Elem::F32).await;
            for elem in [Elem::F32, Elem::F16, Elem::I8] {
                let r = measure(&c, &truth, Some(&f32_exact), elem).await.0;
                println!(
                    "{:<4}  {:>13}  {:>5.3}  {:>10.4}  {:>17.4}  {:>12.4}  {:>13.4}  {:>13.2e}  \
                     {:>13.2}  {:>8.0}",
                    elem.name(),
                    r.bytes,
                    r.bytes as f64 / base.bytes as f64,
                    r.exact,
                    r.rerank,
                    r.ann,
                    r.overlap,
                    r.err,
                    r.scan_ms,
                    r.build_ms
                );
            }
            println!();
        }
    });
}
