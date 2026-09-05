//! Shared fixture: a clustered synthetic corpus with brute-force ground truth.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "a shared test fixture: pub reaches the sibling test binaries, and an index out \
              of range is a bug in the fixture that should name itself immediately"
)]

use pstore_index::cluster::{Clustering, Params};
use pstore_index::ladder::Ladder;
use pstore_index::rabitq::{Code, Quantizer};
use pstore_index::search;
use pstore_index::sq8;

pub struct Rng(pub u64);
impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    }
    pub fn normal(&mut self) -> f32 {
        (0..6).map(|_| self.unit()).sum()
    }
}

/// ⚠️ The cluster tightness is a **measured** choice, not a default. An earlier version used
/// centres four times the noise, giving within-group cosine similarity of 0.97 — vectors so
/// alike that the top-10 for a query differ in the third decimal place, while the 1-bit
/// estimator's error is two orders of magnitude larger. Recall sat at 0.44 and did not move
/// between p=2 and p=32, because the probe was never the bottleneck. That is not a corpus,
/// it is 500 copies of the same vector, and no quantizer can rank it.
///
/// Centres at 1.5x the noise give within-group cosine ~0.69, which is where real embedding
/// corpora sit.
///
/// ⚠️ A Gaussian mixture is also what clustering is *best* at, so a recall number measured
/// only here flatters the implementation. The gate reports its dataset with every figure, and
/// `scripts/recall.sh` also runs a uniform cloud, where there is no structure to find.
pub struct Corpus {
    pub vectors: Vec<Vec<f32>>,
    pub queries: Vec<Vec<f32>>,
    pub quantizer: Quantizer,
}

/// A clustering plus the residual codes it implies.
///
/// ⚠️ Codes depend on the clustering, because each is a residual from the centroid of the
/// list its vector landed in. They cannot be computed once for the corpus and reused across
/// clusterings — which is also why building an index is a single pass that owns both.
pub struct Index {
    pub clustering: Clustering,
    /// Per row: its code, and the list it was coded against.
    pub codes: Vec<(Code, usize)>,
    pub eights: Vec<sq8::Code>,
}

impl Corpus {
    pub fn clustered(n: usize, dim: usize, groups: usize, seed: u64) -> Self {
        let mut rng = Rng(seed);
        let centres: Vec<Vec<f32>> = (0..groups)
            .map(|_| (0..dim).map(|_| rng.normal() * 1.5).collect())
            .collect();
        let vectors: Vec<Vec<f32>> = (0..n)
            .map(|i| {
                let c = &centres[i % groups];
                normalize(c.iter().map(|x| x + rng.normal()).collect())
            })
            .collect();
        // Queries drawn from the same distribution: a query from somewhere else measures
        // out-of-distribution behaviour, which is a different question.
        let queries: Vec<Vec<f32>> = (0..60)
            .map(|i| {
                let c = &centres[i % groups];
                normalize(c.iter().map(|x| x + rng.normal()).collect())
            })
            .collect();
        Self {
            vectors,
            queries,
            quantizer: Quantizer::new(dim),
        }
    }

    /// A uniform cloud: no cluster structure at all, so clustering has nothing to find.
    pub fn uniform(n: usize, dim: usize, seed: u64) -> Self {
        let mut rng = Rng(seed);
        let vectors: Vec<Vec<f32>> = (0..n)
            .map(|_| normalize((0..dim).map(|_| rng.normal()).collect()))
            .collect();
        let queries: Vec<Vec<f32>> = (0..60)
            .map(|_| normalize((0..dim).map(|_| rng.normal()).collect()))
            .collect();
        Self {
            vectors,
            queries,
            quantizer: Quantizer::new(dim),
        }
    }

    pub fn build(&self, params: Params) -> Index {
        let clustering = Clustering::build(&self.vectors, params);
        // Each row is coded against the centroid of the list it was ASSIGNED to. A
        // replicated row keeps one code -- the residual is a property of the vector and its
        // home centroid, not of every list it can be reached through.
        let mut home = vec![0usize; self.vectors.len()];
        for (ci, list) in clustering.lists().iter().enumerate() {
            for r in list {
                home[*r] = ci;
            }
        }
        let codes = self
            .vectors
            .iter()
            .enumerate()
            .map(|(r, v)| {
                let c = &clustering.centroids()[home[r]];
                (self.quantizer.encode_residual(v, c).unwrap(), home[r])
            })
            .collect();
        let eights = self.vectors.iter().map(|v| sq8::encode(v)).collect();
        Index {
            clustering,
            codes,
            eights,
        }
    }

    pub fn dot(&self, row: usize, query: &[f32]) -> f32 {
        self.vectors[row]
            .iter()
            .zip(query)
            .map(|(a, b)| a * b)
            .sum()
    }

    /// Brute-force top-k. The reference every recall number is measured against, computed
    /// from the vectors rather than from the index -- a self-consistent wrong answer is
    /// exactly what a weaker reference would miss.
    pub fn truth(&self, query: &[f32], k: usize) -> Vec<usize> {
        let mut all: Vec<(usize, f32)> = (0..self.vectors.len())
            .map(|i| (i, self.dot(i, query)))
            .collect();
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        all.truncate(k);
        all.into_iter().map(|(i, _)| i).collect()
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

/// recall@k at probe width `p`, through rung 0 then the int8 rung.
///
/// ⚠️ `rerank: fast` is the default, not `none` — measured, and recorded as C-3 against
/// `quantization.md`. Rung 0 alone gives about 0.30 at 384 dimensions, because everything
/// in one posting list is similar by construction and a 1-bit code cannot rank differences
/// smaller than its own error.
pub fn recall_at(c: &Corpus, idx: &Index, k: usize, p: usize, oversample: usize) -> f64 {
    let l = Ladder::new(k, oversample);
    let mut hits = 0usize;
    for query in &c.queries {
        let want = c.truth(query, k);
        let scored = l.rung0(rung0_scores(c, idx, query, p));
        let refined = l.rung1(&scored, |r| sq8::estimate(&idx.eights[r], query));
        hits += want
            .iter()
            .filter(|i| refined.iter().take(k).any(|(j, _)| j == *i))
            .count();
    }
    hits as f64 / (c.queries.len() * k) as f64
}

/// Rung-0 scores for the candidates `p` probes reach.
pub fn rung0_scores(c: &Corpus, idx: &Index, query: &[f32], p: usize) -> Vec<(usize, f32)> {
    let lists = search::probe(&idx.clustering, query, p);
    let cand = search::candidates(&idx.clustering, &lists);
    let prepared = c.quantizer.prepare(query).unwrap();
    // ⚠️ One `<c, q>` per centroid, not per candidate. There are thousands of vectors
    // behind each centroid; computing it per candidate would put a full-precision dot
    // product back on the inner loop the codes exist to avoid.
    let dots: Vec<f32> = idx
        .clustering
        .centroids()
        .iter()
        .map(|cen| cen.iter().zip(query).map(|(a, b)| a * b).sum())
        .collect();
    cand.iter()
        .map(|r| {
            let (code, home) = &idx.codes[*r];
            (
                *r,
                c.quantizer.estimate_residual(code, &prepared, dots[*home]),
            )
        })
        .collect()
}

trait ContainsKey {
    fn contains_key(&self, i: usize) -> bool;
}
impl ContainsKey for Vec<(usize, f32)> {
    fn contains_key(&self, i: usize) -> bool {
        self.iter().any(|(j, _)| *j == i)
    }
}
