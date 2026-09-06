//! **OQ-39**: RaBitQ against BBQ-style variants, on the question that matters.
//!
//! C-3 measured rung 0 — the 1-bit tier alone — at 0.31 recall@10, against D-11's expected
//! 90–95%. The int8 rung rescues it at 0.98, but costs ~4x the bytes per query, and bytes
//! are what bounds QPS/node. So the decisive question is not "which binary scheme is more
//! accurate in the abstract" but:
//!
//!   **Can any 1-bit scheme make rung 0 answer on its own?** If yes, the int8 tier and its
//!   bytes go away. If no, the ladder is structural and OQ-39 is settled.
//!
//! Three variants, differing in exactly what separates RaBitQ from BBQ:
//!
//!   * **rotated, f32 query** — RaBitQ as shipped.
//!   * **unrotated, f32 query** — BBQ's construction: centroid residual, sign bits,
//!     per-vector corrections, no randomized transform.
//!   * **rotated, int4 query** — BBQ's asymmetry. Our query is `f32` today, which is *more*
//!     accurate, so this measures what the throughput win would cost.
//!
//!   cargo run --release -p pstore-index --example oq39
//!
//! ⚠️ `provisional`: WSL2, synthetic corpus.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "a measurement harness: an index out of range is a bug in the harness"
)]

use pstore_index::cluster::{Clustering, Params};
use pstore_index::ladder::Ladder;
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

fn main() {
    let (dim, groups, n) = (384usize, 40usize, 20_000usize);
    let params = Params {
        target_list_size: 200,
        ..Params::default()
    };
    let mut rng = Rng(11);
    let centres: Vec<Vec<f32>> = (0..groups)
        .map(|_| (0..dim).map(|_| rng.normal() * 1.5).collect())
        .collect();
    let make = |rng: &mut Rng, i: usize| {
        norm(
            centres[i % groups]
                .iter()
                .map(|x| x + rng.normal())
                .collect(),
        )
    };
    let corpus: Vec<Vec<f32>> = (0..n).map(|i| make(&mut rng, i)).collect();
    let queries: Vec<Vec<f32>> = (0..100).map(|i| make(&mut rng, i)).collect();
    let truth: Vec<Vec<usize>> = queries
        .iter()
        .map(|q| {
            let mut all: Vec<(usize, f32)> = (0..n).map(|i| (i, dot(&corpus[i], q))).collect();
            all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            all.truncate(10);
            all.into_iter().map(|(i, _)| i).collect()
        })
        .collect();

    let c = Clustering::build(&corpus, params);
    let mut home = vec![0usize; n];
    for (ci, list) in c.lists().iter().enumerate() {
        for r in list {
            home[*r] = ci;
        }
    }
    let eights: Vec<_> = corpus.iter().map(|v| sq8::encode(v)).collect();

    println!(
        "# {n} x {dim}d clustered, {} lists, k=10, p=8\n",
        c.lists().len()
    );
    println!(
        "{:<28} {:>12} {:>14}",
        "variant", "rung 0 only", "with int8 rung"
    );

    for (label, q, int4) in [
        ("RaBitQ (rotated, f32 q)", Quantizer::new(dim), false),
        (
            "BBQ-like (no rotation)",
            Quantizer::without_rotation(dim),
            false,
        ),
        ("RaBitQ + int4 query", Quantizer::new(dim), true),
    ] {
        let codes: Vec<_> = corpus
            .iter()
            .enumerate()
            .map(|(r, v)| q.encode_residual(v, &c.centroids()[home[r]]).unwrap())
            .collect();
        let l = Ladder::new(10, 32);
        let (mut r0, mut r1) = (0usize, 0usize);
        for (qi, query) in queries.iter().enumerate() {
            let lists = search::probe(&c, query, 8);
            let cand = search::candidates(&c, &lists);
            let prepared = {
                let p = q.prepare(query).unwrap();
                if int4 { q.quantize_query_int4(&p) } else { p }
            };
            let dots: Vec<f32> = c.centroids().iter().map(|cen| dot(cen, query)).collect();
            let scored = l.rung0(cand.iter().map(|r| {
                (
                    *r,
                    q.estimate_residual(&codes[*r], &prepared, dots[home[*r]]),
                )
            }));
            r0 += truth[qi]
                .iter()
                .filter(|i| scored.iter().take(10).any(|(j, _)| j == *i))
                .count();
            let refined = l.rung1(&scored, |r| sq8::estimate(&eights[r], query));
            r1 += truth[qi]
                .iter()
                .filter(|i| refined.iter().take(10).any(|(j, _)| j == *i))
                .count();
        }
        let d = (queries.len() * 10) as f64;
        println!(
            "{label:<28} {:>12.4} {:>14.4}",
            r0 as f64 / d,
            r1 as f64 / d
        );
    }
    println!(
        "\n# If no variant lifts 'rung 0 only' near 0.90, the int8 rung is structural and\n\
         # OQ-39 is settled: no binary scheme removes it at these dimensions."
    );
}
