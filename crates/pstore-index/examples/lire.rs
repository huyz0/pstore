//! **The OQ-51 spike.** Does batched LIRE preserve partition quality against a rebuild?
//!
//! The roadmap's exit for M3.5 is a verdict, not a number: either "batched LIRE preserves
//! quality" or "we need a different maintenance strategy". This runs both paths over the
//! same stream of inserts and prints what separates them.
//!
//!   cargo run --release -p pstore-index --example lire
//!
//! ⚠️ `provisional`: WSL2, synthetic corpus, thousands of vectors rather than a billion.
//! The published LIRE result is for a mutable disk index at billion scale; what is being
//! asked here is only whether BATCHING into an immutable rewrite destroys the property.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "a measurement harness: an index out of range is a bug in the harness"
)]

use pstore_index::cluster::{Clustering, Params};
use pstore_index::ladder::Ladder;
use pstore_index::lire::{self, Scope};
use pstore_index::rabitq::Quantizer;
use pstore_index::{search, sq8};

struct Rng(u64);
impl Rng {
    fn f(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    }
    fn normal(&mut self) -> f32 {
        (0..6).map(|_| self.f()).sum()
    }
}

fn norm(mut v: Vec<f32>) -> Vec<f32> {
    let m: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in v.iter_mut() {
        *x /= m;
    }
    v
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// recall@10 through the real ladder, so quality is measured the way a query sees it.
fn recall(
    corpus: &[Vec<f32>],
    c: &Clustering,
    queries: &[Vec<f32>],
    truth: &[Vec<usize>],
    dim: usize,
) -> f64 {
    let q = Quantizer::new(dim);
    let mut home = vec![0usize; corpus.len()];
    for (ci, list) in c.lists().iter().enumerate() {
        for r in list {
            home[*r] = ci;
        }
    }
    let codes: Vec<_> = corpus
        .iter()
        .enumerate()
        .map(|(r, v)| q.encode_residual(v, &c.centroids()[home[r]]).unwrap())
        .collect();
    let eights: Vec<_> = corpus.iter().map(|v| sq8::encode(v)).collect();
    let l = Ladder::new(10, 32);
    let mut hits = 0usize;
    for (qi, query) in queries.iter().enumerate() {
        let lists = search::probe(c, query, 8);
        let cand = search::candidates(c, &lists);
        let prepared = q.prepare(query).unwrap();
        let dots: Vec<f32> = c.centroids().iter().map(|cen| dot(cen, query)).collect();
        let scored = l.rung0(cand.iter().map(|r| {
            (
                *r,
                q.estimate_residual(&codes[*r], &prepared, dots[home[*r]]),
            )
        }));
        let r1 = l.rung1(&scored, |r| sq8::estimate(&eights[r], query));
        hits += truth[qi]
            .iter()
            .filter(|i| r1.iter().take(10).any(|(j, _)| j == *i))
            .count();
    }
    hits as f64 / (queries.len() * 10) as f64
}

fn report(label: &str, corpus: &[Vec<f32>], c: &Clustering, r: f64, examined: usize, moved: usize) {
    let sizes: Vec<usize> = c.lists().iter().map(Vec::len).collect();
    let entries: usize = sizes.iter().sum();
    let mean = entries as f64 / sizes.len().max(1) as f64;
    let largest = sizes.iter().copied().max().unwrap_or(0);
    println!(
        "{label:<26} lists={:<4} max/mean={:<5.2} cost/floor={:<5.3} recall@10={r:.4} \
         examined={examined:<7} moved={moved}",
        sizes.len(),
        largest as f64 / mean,
        c.assignment_cost(corpus) / c.unconstrained_cost(corpus).max(1e-9),
    );
}

fn main() {
    let (dim, groups, initial, batches, per_batch) =
        (64usize, 20usize, 6_000usize, 10usize, 400usize);
    // ⚠️ `replicas: 0`: maintenance operates on the assignment. Boundary replication is a
    // build-time step applied when the segment is written, and feeding augmented lists to
    // `maintain` makes every centroid move on the first recentre, so every list looks
    // disturbed and the scope restriction stops paying.
    let params = Params {
        target_list_size: 150,
        replicas: 0,
        ..Params::default()
    };
    let mut rng = Rng(7);
    let centres: Vec<Vec<f32>> = (0..groups)
        .map(|_| (0..dim).map(|_| rng.normal() * 1.5).collect())
        .collect();
    let total = initial + batches * per_batch;
    // ⚠️ The inserts DRIFT: later batches favour later groups, so the distribution the
    // index was built for is not the one it ends up serving. A stationary stream would let
    // any maintenance strategy look fine, and drift is the case the protocol exists for.
    let corpus: Vec<Vec<f32>> = (0..total)
        .map(|i| {
            let g = if i < initial {
                i % groups
            } else {
                let progress = (i - initial) * groups / (total - initial);
                (groups / 2 + progress) % groups
            };
            norm(centres[g].iter().map(|x| x + rng.normal()).collect())
        })
        .collect();
    let queries: Vec<Vec<f32>> = (0..100)
        .map(|i| {
            norm(
                centres[i % groups]
                    .iter()
                    .map(|x| x + rng.normal())
                    .collect(),
            )
        })
        .collect();
    let truth: Vec<Vec<usize>> = queries
        .iter()
        .map(|q| {
            let mut all: Vec<(usize, f32)> = (0..total).map(|i| (i, dot(&corpus[i], q))).collect();
            all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            all.truncate(10);
            all.into_iter().map(|(i, _)| i).collect()
        })
        .collect();

    println!(
        "# {total} x {dim}d, {groups} groups, {initial} initial + {batches} batches of \
         {per_batch}, drifting distribution\n"
    );

    // Both scopes, so the cost of restricting reassignment to the disturbed lists is
    // visible rather than assumed.
    let seed_corpus: Vec<Vec<f32>> = corpus[..initial].to_vec();
    for (label, scope, split_at) in [
        ("LIRE split@4x target", Scope::Touched, 4.0f32),
        ("LIRE split@2x target", Scope::Touched, 2.0),
        ("LIRE split@1.5x target", Scope::Touched, 1.5),
        ("LIRE full scan @2x", Scope::All, 2.0),
    ] {
        let mut c = Clustering::build(&seed_corpus, params);
        let (mut examined, mut moved) = (0usize, 0usize);
        let mut at = initial;
        for _ in 0..batches {
            let rows: Vec<usize> = (at..at + per_batch).collect();
            at += per_batch;
            let w = lire::maintain_with(
                &mut c,
                &corpus[..at],
                &rows,
                params,
                scope,
                lire::Bounds::with_split_factor(params, split_at),
            );
            examined += w.examined;
            moved += w.reassigned;
        }
        report(
            label,
            &corpus,
            &c,
            recall(&corpus, &c, &queries, &truth, dim),
            examined,
            moved,
        );
    }

    // The rebuild path: throw it all away and cluster the final corpus.
    let rebuilt = Clustering::build(&corpus, params);
    report(
        "rebuild (reference)",
        &corpus,
        &rebuilt,
        recall(&corpus, &rebuilt, &queries, &truth, dim),
        total * batches,
        total,
    );
    println!(
        "\n# examined = vectors looked at across all batches; moved = vectors that changed\n\
         # list. A rebuild examines the whole corpus every batch by definition."
    );
}
