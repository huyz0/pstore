//! The recall gate (D-35), and the sweep behind it.
//!
//! ⚠️ Runs **outside `cargo test`**, deliberately. A gate-scale corpus inside the unit suite
//! would be rebuilt once per mutant by `cargo mutants`, and the suite is run hundreds of
//! times a session. The spec says this; putting a 20,000-vector clustering in a `#[test]`
//! was a mistake that cost minutes per run before it was moved here.
//!
//! Every figure printed carries its **scale, dimension, dataset and parameters**, because a
//! recall number without them is not a measurement (`evaluation-methodology.md`).
//!
//!   cargo run --release -p pstore-index --example recall -- [--sweep]
//!
//! Exit status is the gate: non-zero if recall is under the floor.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "a measurement harness: an index out of range is a bug in the harness, and               panicking names it immediately"
)]

use pstore_index::cluster::{Clustering, Params};
use pstore_index::ladder::Ladder;
use pstore_index::rabitq::{Code, Quantizer};
use pstore_index::search;
use pstore_index::sq8;

const FLOOR: f64 = 0.90;

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
        for x in v.iter_mut() {
            *x /= n;
        }
    }
    v
}

struct Corpus {
    name: &'static str,
    vectors: Vec<Vec<f32>>,
    queries: Vec<Vec<f32>>,
    /// ⚠️ Brute-force top-k, computed **once**. It does not depend on `p`, `oversample`, or
    /// anything else the sweep varies, and recomputing it per configuration made the sweep
    /// spend almost all its time re-deriving the same answer.
    truth: Vec<Vec<usize>>,
    dim: usize,
}

impl Corpus {
    /// A Gaussian mixture: what clustering is best at, and the flattering case.
    fn clustered(n: usize, dim: usize, groups: usize, seed: u64) -> Self {
        let mut rng = Rng(seed);
        let centres: Vec<Vec<f32>> = (0..groups)
            .map(|_| (0..dim).map(|_| rng.normal() * 1.5).collect())
            .collect();
        let make = |rng: &mut Rng, i: usize| {
            let c = &centres[i % groups];
            normalize(c.iter().map(|x| x + rng.normal()).collect())
        };
        let vectors = (0..n).map(|i| make(&mut rng, i)).collect();
        let queries = (0..100).map(|i| make(&mut rng, i)).collect();
        Self::finish("clustered", vectors, queries, dim)
    }

    fn finish(
        name: &'static str,
        vectors: Vec<Vec<f32>>,
        queries: Vec<Vec<f32>>,
        dim: usize,
    ) -> Self {
        let mut c = Self {
            name,
            vectors,
            queries,
            truth: Vec::new(),
            dim,
        };
        c.truth = c.queries.iter().map(|q| c.brute_force(q, 10)).collect();
        c
    }

    /// ⚠️ No cluster structure at all. Included because a gate built only on a mixture
    /// measures the generator as much as the index, and this is the case where clustering
    /// has nothing to find.
    fn uniform(n: usize, dim: usize, seed: u64) -> Self {
        let mut rng = Rng(seed);
        let vectors = (0..n)
            .map(|_| normalize((0..dim).map(|_| rng.normal()).collect()))
            .collect();
        let queries = (0..100)
            .map(|_| normalize((0..dim).map(|_| rng.normal()).collect()))
            .collect();
        Self::finish("uniform", vectors, queries, dim)
    }

    fn dot(&self, row: usize, q: &[f32]) -> f32 {
        self.vectors[row].iter().zip(q).map(|(a, b)| a * b).sum()
    }

    fn brute_force(&self, q: &[f32], k: usize) -> Vec<usize> {
        let mut all: Vec<(usize, f32)> = (0..self.vectors.len())
            .map(|i| (i, self.dot(i, q)))
            .collect();
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        all.truncate(k);
        all.into_iter().map(|(i, _)| i).collect()
    }
}

struct Index {
    clustering: Clustering,
    codes: Vec<(Code, usize)>,
    eights: Vec<sq8::Code>,
    quantizer: Quantizer,
}

fn build(c: &Corpus, params: Params) -> Index {
    let clustering = Clustering::build(&c.vectors, params);
    let mut home = vec![0usize; c.vectors.len()];
    for (ci, list) in clustering.lists().iter().enumerate() {
        for r in list {
            home[*r] = ci;
        }
    }
    let quantizer = Quantizer::new(c.dim);
    let codes = c
        .vectors
        .iter()
        .enumerate()
        .map(|(r, v)| {
            let cen = &clustering.centroids()[home[r]];
            (quantizer.encode_residual(v, cen).unwrap(), home[r])
        })
        .collect();
    let eights = c.vectors.iter().map(|v| sq8::encode(v)).collect();
    Index {
        clustering,
        codes,
        eights,
        quantizer,
    }
}

/// Which rung the answer is taken from.
#[derive(Clone, Copy, PartialEq)]
enum Rerank {
    /// The answer is rung 0's top-k. **3 round trips.** Oversample is irrelevant here --
    /// nothing re-scores, so widening the candidate list changes nothing.
    None,
    /// Rung 0 oversamples, then int8 re-scores the survivors.
    ///
    /// ⚠️ **Still 3 round trips**, if the `sq8` byte ranges for the probed lists are fetched
    /// alongside the `rabitq` ranges — same object, same round, both spans known before
    /// either is issued. It costs bytes, not depth.
    Int8,
    /// Rung 0 oversamples, then full precision re-scores the survivors. **4 round trips**,
    /// because the float32 rows cannot be chosen until rung 0 has ranked.
    Exact,
}

/// recall@k, and the mean number of candidates rung 0 scored.
fn measure(
    c: &Corpus,
    idx: &Index,
    k: usize,
    p: usize,
    oversample: usize,
    rerank: Rerank,
) -> (f64, f64) {
    let l = Ladder::new(k, oversample);
    let mut hits = 0usize;
    let mut cands = 0usize;
    for (qi, q) in c.queries.iter().enumerate() {
        let want = &c.truth[qi];
        let lists = search::probe(&idx.clustering, q, p);
        let cand = search::candidates(&idx.clustering, &lists);
        cands += cand.len();
        let prepared = idx.quantizer.prepare(q).unwrap();
        let dots: Vec<f32> = idx
            .clustering
            .centroids()
            .iter()
            .map(|cen| cen.iter().zip(q).map(|(a, b)| a * b).sum())
            .collect();
        let scored = l.rung0(cand.iter().map(|r| {
            let (code, home) = &idx.codes[*r];
            (
                *r,
                idx.quantizer
                    .estimate_residual(code, &prepared, dots[*home]),
            )
        }));
        let refined = match rerank {
            Rerank::None => scored.iter().take(k).copied().collect(),
            Rerank::Int8 => {
                let r1 = l.rung1(&scored, |r| sq8::estimate(&idx.eights[r], q));
                r1.iter().take(k).copied().collect()
            }
            Rerank::Exact => l.rung2(&scored, |r| c.dot(r, q)),
        };
        hits += want
            .iter()
            .filter(|i| refined.iter().any(|(j, _)| j == *i))
            .count();
    }
    (
        hits as f64 / (c.queries.len() * k) as f64,
        cands as f64 / c.queries.len() as f64,
    )
}

fn main() {
    let sweep = std::env::args().any(|a| a == "--sweep");
    let dim = 384;
    // ⚠️ 20,000, not the spec's 250,000. Brute-force ground truth is O(n * queries * dim)
    // and the whole gate must fit a 5-minute budget on WSL2; 250,000 x 384d takes longer
    // than that in ground truth alone. The number is stated with every result rather than
    // rounded up in prose, and closing the gap needs the machine M0b provides.
    let n = 20_000;
    let params = Params {
        target_list_size: 200,
        ..Params::default()
    };

    // ⚠️ **Item 14's question, and it is a cost question rather than a recall one.** M5g
    // clamped `replicas: 0` in the engine because a replicated vector is written to the
    // segment twice, which breaks `Engine::scan`'s "exactly once" -- at a measured cost of
    // r@10 p=2 0.961 -> 0.844. Recovering it needs list membership that does not duplicate a
    // row, which is format surgery.
    //
    // But replication is only ONE way to buy recall at small `p`; probing more lists is the
    // other, and it needs no format change at all. So before building anything: for a target
    // recall, is it cheaper in **bytes per query** to replicate, or to probe wider?
    //
    //   cargo run --release -p pstore-index --example recall -- --replicas
    if std::env::args().any(|a| a == "--replicas") {
        // ⚠️ **Both corpora.** The clustered one is a Gaussian mixture, which is the case
        // clustering handles best -- `Query::default()`'s own comment warns that taking a
        // number from the flattering case is fitting a default to the generator. Replication
        // exists for vectors sitting between centroids, so uniform data is where it should
        // earn its keep if anywhere.
        for c in [Corpus::clustered(n, dim, 40, 1), Corpus::uniform(n, dim, 2)] {
            println!(
                "# {} x {dim}d {} corpus, k=10, rerank=int8, over=32",
                c.vectors.len(),
                c.name
            );
            println!(
                "{:>9} {:>7} {:>5} {:>9} {:>10} {:>9}",
                "replicas", "bound", "p", "index x", "recall@10", "MB/query"
            );
            // The unreplicated index's entry count is the denominator: "index x" is what
            // replication costs in stored codes, which is a per-segment byte cost forever.
            let base: usize = build(
                &c,
                Params {
                    replicas: 0,
                    ..params
                },
            )
            .clustering
            .lists()
            .iter()
            .map(Vec::len)
            .sum();
            for (replicas, boundary) in [(0usize, 0.0f32), (1, 0.05), (1, 0.10), (2, 0.10)] {
                let idx = build(
                    &c,
                    Params {
                        replicas,
                        boundary,
                        ..params
                    },
                );
                let entries: usize = idx.clustering.lists().iter().map(Vec::len).sum();
                let size = entries as f64 / base as f64;
                for p in [2usize, 4, 8, 16] {
                    let (recall, cands) = measure(&c, &idx, 10, p, 32, Rerank::Int8);
                    // Rung 0 reads the 1-bit codes of every candidate, then int8 for the same
                    // set: the bytes a query actually moves, which is what the cost model prices.
                    let rung0 = cands * (dim.next_power_of_two() / 8 + 8) as f64;
                    let mb = (rung0 + cands * (dim + 8) as f64) / 1e6;
                    println!(
                        "{replicas:>9} {boundary:>7.2} {p:>5} {size:>9.2} {recall:>10.4} {mb:>9.3}"
                    );
                }
            }
        }
        return;
    }

    if sweep {
        // ⚠️ The point of the sweep: rung-0 oversample costs NO bytes and NO round trips.
        // Every candidate it keeps was already scored from posting lists the query fetched
        // anyway; oversample only widens the list handed to the next rung. So it is the one
        // recall knob that is nearly free, and picking it "mid-range" without measuring is
        // leaving recall on the table for nothing.
        for c in [Corpus::clustered(n, dim, 40, 1), Corpus::uniform(n, dim, 2)] {
            let idx = build(&c, params);
            println!(
                "# {} x {dim}d {} corpus, {} lists, k=10",
                c.vectors.len(),
                c.name,
                idx.clustering.lists().len()
            );
            println!(
                "{:>4} {:>6} {:>10} {:>10} {:>10} {:>10}",
                "p", "over", "none(3rt)", "int8(3rt)", "exact(4rt)", "cands"
            );
            for p in [2usize, 8, 16, 32] {
                for over in [8usize, 32] {
                    let (none, cands) = measure(&c, &idx, 10, p, over, Rerank::None);
                    let (i8r, _) = measure(&c, &idx, 10, p, over, Rerank::Int8);
                    let (exact, _) = measure(&c, &idx, 10, p, over, Rerank::Exact);
                    println!(
                        "{p:>4} {over:>6} {none:>10.4} {i8r:>10.4} {exact:>10.4} {cands:>10.0}"
                    );
                }
            }
        }
        return;
    }

    let mut failed = false;
    for c in [
        Corpus::clustered(n, dim, 40, 1),
        Corpus::uniform(10_000, dim, 2),
    ] {
        let idx = build(&c, params);
        // ⚠️ Swept over `p` at the gate, not just reported at one value. `p` is the only
        // knob that drives BOTH recall and bytes, so a default picked without seeing the
        // curve is a default picked from the corpus's range rather than from this index.
        for probe in [4usize, 8, 16] {
            let (r_none, cands) = measure(&c, &idx, 10, probe, 32, Rerank::None);
            let (r_fast, _) = measure(&c, &idx, 10, probe, 32, Rerank::Int8);
            let (r_exact, _) = measure(&c, &idx, 10, probe, 32, Rerank::Exact);
            let survivors = (10 * 32) as f64;
            let rung0 = cands * (dim.next_power_of_two() / 8 + 8) as f64;
            println!(
                "  p={probe:<3} cands={cands:<6.0} none={r_none:.4}/{:.2}MB  \
                 fast={r_fast:.4}/{:.2}MB  exact={r_exact:.4}/{:.2}MB",
                rung0 / 1e6,
                (rung0 + cands * (dim + 8) as f64) / 1e6,
                (rung0 + survivors * (dim * 4) as f64) / 1e6
            );
        }
        let (recall, cands) = measure(&c, &idx, 10, 16, 32, Rerank::Int8);
        let lists = idx.clustering.lists().len();
        // ⚠️ Bytes per query, per rerank mode, because that is the cost model's dominant
        // input: `cost-model.md` prices a node on *vectors scanned per second*, and QPS/node
        // swings 75x with scan size. A recall number without it is half a measurement.
        //
        //   rung 0  : every candidate's 1-bit code + its two scalars
        //   fast    : + int8 for every CANDIDATE, because the ranges must be chosen before
        //             rung 0 has ranked if it is to stay in the same round trip
        //   exact   : + float32 for the SURVIVORS only, in a fourth round
        let survivors = (10 * 32) as f64;
        let rung0 = cands * (dim.next_power_of_two() / 8 + 8) as f64;
        let fast = rung0 + cands * (dim + 8) as f64;
        let exact = rung0 + survivors * (dim * 4) as f64;
        println!(
            "{:>10}  n={:<7} dim={dim}  lists={lists:<4} p=16 over=32  cands={cands:.0}",
            c.name,
            c.vectors.len()
        );
        for (mode, r, mb) in [
            (
                "none  (3rt)",
                measure(&c, &idx, 10, 16, 32, Rerank::None).0,
                rung0,
            ),
            ("fast  (3rt)", recall, fast),
            (
                "exact (4rt)",
                measure(&c, &idx, 10, 16, 32, Rerank::Exact).0,
                exact,
            ),
        ] {
            println!(
                "             {mode}  recall@10={r:.4}  bytes/query={:.2} MB",
                mb / 1e6
            );
        }
        if c.name == "clustered" && recall < FLOOR {
            eprintln!("FAIL recall@10 {recall:.4} is under the {FLOOR:.2} floor");
            failed = true;
        }
    }
    if failed {
        std::process::exit(1);
    }
    println!("recall gate: PASS (provisional: WSL2, synthetic corpus)");
}
