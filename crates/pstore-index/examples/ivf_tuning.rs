//! M283: lucene-rust's IVF tuning findings, measured on pstore's index (lesson 7 of
//! `docs/comparisons/lessons-from-hsearch-opensearch.md`).
//!
//!   cargo run --release -p pstore-index --example ivf_tuning
//!
//! ⚠️ `provisional`: a cloud container, synthetic corpora, the recall gate's scale (20,000 x
//! 384d, lists of 200). Relative numbers only. Three questions, each printed with its
//! parameters:
//!
//! 1. **Centroid ranking's share of a query's CPU**, at the gate's 100 lists and at
//!    lucene-rust's 6,437, beside the scoring of the probed lists' codes.
//! 2. **An unconstrained final Lloyd round** after the capacity-bounded k-means: candidate
//!    recall at a fixed `p`, candidates read, the largest list, and how many vectors move.
//! 3. **Filtered probing**: how many of a segment's lists hold an admitted row, so how many a
//!    filtered dense leg could skip, for random and for vector-correlated filters.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "a measurement harness: an index out of range is a bug in the harness"
)]

use pstore_index::cluster::{Clustering, Params};
use pstore_index::rabitq::{Code, Quantizer};
use pstore_index::search;
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

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

struct Corpus {
    name: &'static str,
    vectors: Vec<Vec<f32>>,
    /// Each row's mixture component, or 0: what a vector-correlated filter selects on.
    group: Vec<usize>,
    queries: Vec<Vec<f32>>,
    truth: Vec<Vec<usize>>,
}

impl Corpus {
    fn clustered(n: usize, dim: usize, groups: usize, seed: u64) -> Self {
        let mut rng = Rng(seed);
        let centres: Vec<Vec<f32>> = (0..groups)
            .map(|_| (0..dim).map(|_| rng.normal() * 1.5).collect())
            .collect();
        let make = |rng: &mut Rng, i: usize| {
            normalize(
                centres[i % groups]
                    .iter()
                    .map(|x| x + rng.normal())
                    .collect(),
            )
        };
        let vectors = (0..n).map(|i| make(&mut rng, i)).collect();
        let queries = (0..100).map(|i| make(&mut rng, i)).collect();
        Self::finish(
            "clustered",
            vectors,
            (0..n).map(|i| i % groups).collect(),
            queries,
        )
    }

    fn uniform(n: usize, dim: usize, seed: u64) -> Self {
        let mut rng = Rng(seed);
        let mut v = || normalize((0..dim).map(|_| rng.normal()).collect());
        let vectors = (0..n).map(|_| v()).collect();
        let queries = (0..100).map(|_| v()).collect();
        Self::finish("uniform", vectors, vec![0; n], queries)
    }

    fn finish(
        name: &'static str,
        vectors: Vec<Vec<f32>>,
        group: Vec<usize>,
        queries: Vec<Vec<f32>>,
    ) -> Self {
        let truth = queries
            .iter()
            .map(|q| {
                let mut all: Vec<(usize, f32)> = vectors
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (i, dot(v, q)))
                    .collect();
                all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                all.into_iter().take(10).map(|(i, _)| i).collect()
            })
            .collect();
        Self {
            name,
            vectors,
            group,
            queries,
            truth,
        }
    }
}

fn nearest(v: &[f32], centroids: &[Vec<f32>]) -> usize {
    let d2 = |c: &Vec<f32>| v.iter().zip(c).map(|(a, b)| (a - b) * (a - b)).sum::<f32>();
    (0..centroids.len())
        .min_by(|a, b| d2(&centroids[*a]).total_cmp(&d2(&centroids[*b])))
        .unwrap()
}

fn assign_nearest(vectors: &[Vec<f32>], centroids: &[Vec<f32>]) -> Vec<Vec<usize>> {
    let mut lists = vec![Vec::new(); centroids.len()];
    for (r, v) in vectors.iter().enumerate() {
        lists[nearest(v, centroids)].push(r);
    }
    lists
}

/// lucene-rust's final round: every vector to its nearest centroid, ignoring the cap, the
/// centroids recomputed, and every vector reassigned once more.
fn lloyd_round(vectors: &[Vec<f32>], base: &Clustering) -> (Clustering, usize) {
    let dim = vectors[0].len();
    let first = assign_nearest(vectors, base.centroids());
    let centroids: Vec<Vec<f32>> = first
        .iter()
        .zip(base.centroids())
        .map(|(list, old)| {
            if list.is_empty() {
                return old.clone();
            }
            let mut sum = vec![0f32; dim];
            for r in list {
                for (a, x) in sum.iter_mut().zip(&vectors[*r]) {
                    *a += x;
                }
            }
            sum.into_iter().map(|s| s / list.len() as f32).collect()
        })
        .collect();
    let lists = assign_nearest(vectors, &centroids);
    let mut home = vec![0usize; vectors.len()];
    for (c, list) in base.lists().iter().enumerate() {
        for r in list {
            home[*r] = c;
        }
    }
    let moved = lists
        .iter()
        .enumerate()
        .map(|(c, l)| l.iter().filter(|r| home[**r] != c).count())
        .sum();
    (Clustering::from_parts(centroids, lists), moved)
}

/// Mean fraction of the true top 10 among the candidates of `p` probed lists, and mean
/// candidates a query.
fn candidate_recall(c: &Corpus, cl: &Clustering, p: usize) -> (f64, f64) {
    let (mut hit, mut read) = (0usize, 0usize);
    for (q, truth) in c.queries.iter().zip(&c.truth) {
        let lists = search::probe(cl, q, p);
        let cands = search::candidates(cl, &lists);
        read += cands.len();
        hit += truth
            .iter()
            .filter(|t| cands.binary_search(t).is_ok())
            .count();
    }
    let n = c.queries.len() as f64;
    (hit as f64 / (n * 10.0), read as f64 / n)
}

fn largest(cl: &Clustering) -> (usize, f64) {
    let max = cl.lists().iter().map(Vec::len).max().unwrap_or(0);
    let mean = cl.lists().iter().map(Vec::len).sum::<usize>() as f64 / cl.lists().len() as f64;
    (max, max as f64 / mean)
}

/// Question 1: microseconds a query to rank `centroids`, and to score the codes of `p`
/// probed lists of `list_len` rows each.
fn ranking_share(centroids: &[Vec<f32>], dim: usize, p: usize, list_len: usize) -> (f64, f64) {
    let mut rng = Rng(9);
    let quantizer = Quantizer::new(dim);
    let rows: Vec<Vec<f32>> = (0..p * list_len)
        .map(|_| normalize((0..dim).map(|_| rng.normal()).collect()))
        .collect();
    let codes: Vec<Code> = rows
        .iter()
        .map(|v| quantizer.encode_residual(v, &centroids[0]).unwrap())
        .collect();
    let queries: Vec<Vec<f32>> = (0..50)
        .map(|_| normalize((0..dim).map(|_| rng.normal()).collect()))
        .collect();
    let t = Instant::now();
    let mut sink = 0usize;
    for q in &queries {
        sink += search::probe_centroids(centroids, q, p).len();
    }
    let rank = t.elapsed().as_secs_f64() * 1e6 / queries.len() as f64;
    let t = Instant::now();
    let mut acc = 0f32;
    for q in &queries {
        let prepared = quantizer.prepare(q).unwrap();
        let cd = dot(&centroids[0], q);
        for code in &codes {
            acc += quantizer.estimate_residual(code, &prepared, cd);
        }
    }
    let score = t.elapsed().as_secs_f64() * 1e6 / queries.len() as f64;
    std::hint::black_box((sink, acc));
    (rank, score)
}

fn main() {
    let (n, dim) = (20_000, 384);
    let params = Params {
        target_list_size: 200,
        // The engine builds without replicas (M5g), so the comparison does too.
        replicas: 0,
        ..Params::default()
    };
    let corpora = [Corpus::clustered(n, dim, 40, 1), Corpus::uniform(n, dim, 2)];

    println!("# 1. centroid ranking against code scoring, {dim}d, p=8, per query (provisional)");
    println!(
        "{:>10} {:>10} {:>12} {:>12} {:>8}",
        "centroids", "list rows", "rank us", "score us", "share"
    );
    let mut rng = Rng(5);
    for (k, list_len) in [(100, 200), (100, 4_000), (6_437, 200), (6_437, 4_000)] {
        let centroids: Vec<Vec<f32>> = (0..k)
            .map(|_| normalize((0..dim).map(|_| rng.normal()).collect()))
            .collect();
        let (rank, score) = ranking_share(&centroids, dim, 8, list_len);
        println!(
            "{k:>10} {list_len:>10} {rank:>12.1} {score:>12.1} {:>7.1}%",
            100.0 * rank / (rank + score)
        );
    }

    println!();
    println!("# 2. an unconstrained final Lloyd round, {n} x {dim}d, lists of 200, no replicas");
    println!(
        "{:>10} {:>8} {:>3} {:>10} {:>11} {:>9} {:>8} {:>7}",
        "corpus", "build", "p", "recall", "candidates", "max list", "max/mean", "moved"
    );
    for c in &corpora {
        let t = Instant::now();
        let base = Clustering::build(&c.vectors, params);
        let base_s = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let (lloyd, moved) = lloyd_round(&c.vectors, &base);
        let lloyd_s = t.elapsed().as_secs_f64();
        for (name, cl, secs, moved) in [
            ("bounded", &base, base_s, 0),
            ("+ lloyd", &lloyd, base_s + lloyd_s, moved),
        ] {
            let (max, ratio) = largest(cl);
            for p in [2, 4, 8] {
                let (recall, read) = candidate_recall(c, cl, p);
                println!(
                    "{:>10} {:>7.2}s {p:>3} {recall:>10.3} {read:>11.0} {max:>9} {ratio:>8.2} {:>6.1}%",
                    format!("{} {name}", &c.name[..4]),
                    secs,
                    100.0 * moved as f64 / n as f64,
                );
            }
        }
    }

    println!();
    println!("# 3. filtered probing: lists holding an admitted row, {n} rows, lists of 200");
    // What a filtered dense leg reads today, every list's codes and int8 records, against
    // reading the admitted rows' vectors, which needs the mask before the vectors.
    let today = n * (Quantizer::new(dim).code_len() + pstore_index::sq8::record_len(dim));
    println!(
        "#    today every list is read: {:.2} MB a segment",
        today as f64 / 1e6
    );
    println!(
        "{:>10} {:>12} {:>9} {:>7} {:>10} {:>12}",
        "corpus", "filter", "admitted", "lists", "skippable", "exact MB"
    );
    for c in &corpora {
        let cl = Clustering::build(&c.vectors, params);
        let mut home = vec![0usize; n];
        for (li, list) in cl.lists().iter().enumerate() {
            for r in list {
                home[*r] = li;
            }
        }
        let lists = cl.lists().len();
        let touched = |admit: &dyn Fn(usize) -> bool| {
            let mut hit = vec![false; lists];
            let mut count = 0;
            for r in (0..n).filter(|r| admit(*r)) {
                hit[home[r]] = true;
                count += 1;
            }
            (count, hit.iter().filter(|h| **h).count())
        };
        let mut rows = Vec::new();
        for a in [10usize, 100, 300, 1_000, 5_000] {
            // A hash of the row: a filter uncorrelated with the vectors. (A stride is not:
            // the clustered corpus assigns rows to components round robin.)
            let admit = |r: usize| (Rng(r as u64).next_u64() % n as u64) < a as u64;
            rows.push((format!("random {a}"), touched(&admit)));
        }
        if c.name == "clustered" {
            // One mixture component: a filter that follows the vectors.
            rows.push(("one group".to_owned(), touched(&|r| c.group[r] == 0)));
        }
        for (filter, (count, hit)) in rows {
            println!(
                "{:>10} {filter:>12} {count:>9} {hit:>4}/{lists:<3} {:>9.0}% {:>12.3}",
                c.name,
                100.0 * (lists - hit) as f64 / lists as f64,
                (count * dim * 4) as f64 / 1e6
            );
        }
    }
}
