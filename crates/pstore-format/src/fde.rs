//! Fixed-dimensional encodings of multi-vector sets (M67, MUVERA).
//!
//! A set of vectors becomes one vector whose dot product with another set's approximates their
//! Chamfer similarity (the sum over the query's vectors of the best dot product with the
//! document's). `R` repetitions each partition space by SimHash into `2^k` buckets, aggregate
//! each set's vectors per bucket -- the **query** by sum, the **document** by mean, an empty
//! bucket taking the document's vector nearest by Hamming distance -- project each bucket
//! to `d_proj` dimensions by a random sign matrix, and concatenate.
//!
//! ⚠️ **The randomness is a fixed function of `(seed, repetition, dimension)`**, so a stored
//! encoding and a later query agree on every node and across restarts. Changing [`DEFAULT`]
//! invalidates every stored encoding.

/// The parameters of an encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// Repetitions of the partition.
    pub reps: usize,
    /// SimHash bits: `2^k_sim` buckets a repetition.
    pub k_sim: usize,
    /// Dimensions each bucket is projected to; 0 keeps the input's.
    pub d_proj: usize,
    /// The seed every random choice derives from.
    pub seed: u64,
}

/// The parameters the server writes and the `fde` leg queries with: 4 repetitions of 8
/// buckets of 16 dimensions, 512 floats a set.
pub const DEFAULT: Params = Params {
    reps: 4,
    k_sim: 3,
    d_proj: 16,
    seed: 0x6d75_7665_7261_0001,
};

/// A name's suffix that marks a derived encoding field (M67).
pub const FIELD_SUFFIX: &str = "$fde";

/// The derived field holding `name`'s encoding.
#[must_use]
pub fn field_name(name: &str) -> String {
    format!("{name}{FIELD_SUFFIX}")
}

impl Params {
    /// The encoding's width for vectors of `d` dimensions.
    #[must_use]
    pub fn dim(&self, d: usize) -> usize {
        let per = if self.d_proj == 0 { d } else { self.d_proj };
        self.reps * (1usize << self.k_sim) * per
    }
}

fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A hash of the seed and four coordinates.
fn hash(seed: u64, a: u64, b: u64, c: u64, d: u64) -> u64 {
    mix(mix(mix(mix(seed ^ a) ^ b) ^ c) ^ d)
}

/// A standard normal from two hashes (Box-Muller).
fn gauss(seed: u64, rep: usize, bit: usize, i: usize) -> f64 {
    let u = |n: u64| -> f64 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "53 bits of a hash scaled to the open unit interval"
        )]
        let f = ((hash(seed, rep as u64, bit as u64, i as u64, n) >> 11) as f64 + 0.5)
            / (1u64 << 53) as f64;
        f
    };
    (-2.0 * u(0).ln()).sqrt() * (std::f64::consts::TAU * u(1)).cos()
}

/// The projection's sign for output `j` of input `i`.
fn sign(seed: u64, rep: usize, j: usize, i: usize) -> f64 {
    if hash(seed, rep as u64, j as u64, i as u64, 7) & 1 == 0 {
        1.0
    } else {
        -1.0
    }
}

fn dot(a: &[f32], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| f64::from(*x) * y).sum()
}

/// Which bucket `v` falls in, in repetition `rep`.
fn bucket(planes: &[Vec<f64>], v: &[f32]) -> usize {
    planes
        .iter()
        .enumerate()
        .fold(0, |b, (i, g)| b | (usize::from(dot(v, g) > 0.0) << i))
}

fn encode(p: &Params, vectors: &[Vec<f32>], doc: bool) -> Vec<f32> {
    let Some(d) = vectors.first().map(Vec::len) else {
        return Vec::new();
    };
    let vectors: Vec<&Vec<f32>> = vectors.iter().filter(|v| v.len() == d).collect();
    let buckets = 1usize << p.k_sim;
    let per = if p.d_proj == 0 { d } else { p.d_proj };
    let mut out = Vec::with_capacity(p.reps * buckets * per);
    for rep in 0..p.reps {
        let planes: Vec<Vec<f64>> = (0..p.k_sim)
            .map(|bit| (0..d).map(|i| gauss(p.seed, rep, bit, i)).collect())
            .collect();
        let of: Vec<usize> = vectors.iter().map(|v| bucket(&planes, v)).collect();
        for b in 0..buckets {
            let members: Vec<&&Vec<f32>> = vectors
                .iter()
                .zip(&of)
                .filter(|(_, o)| **o == b)
                .map(|(v, _)| v)
                .collect();
            let mut acc = vec![0.0f64; d];
            if members.is_empty() {
                if doc {
                    // The vector whose bucket is nearest by Hamming distance, earliest first.
                    let nearest = of
                        .iter()
                        .enumerate()
                        .min_by_key(|(i, o)| ((**o ^ b).count_ones(), *i))
                        .and_then(|(i, _)| vectors.get(i));
                    if let Some(v) = nearest {
                        for (a, x) in acc.iter_mut().zip(v.iter()) {
                            *a = f64::from(*x);
                        }
                    }
                }
            } else {
                for v in &members {
                    for (a, x) in acc.iter_mut().zip(v.iter()) {
                        *a += f64::from(*x);
                    }
                }
                if doc {
                    #[expect(clippy::cast_precision_loss, reason = "a count of vectors")]
                    let n = members.len() as f64;
                    for a in &mut acc {
                        *a /= n;
                    }
                }
            }
            if p.d_proj == 0 {
                out.extend(acc.iter().map(|a| *a as f32));
            } else {
                #[expect(clippy::cast_precision_loss, reason = "a dimension count")]
                let scale = 1.0 / (p.d_proj as f64).sqrt();
                for j in 0..p.d_proj {
                    let s: f64 = acc
                        .iter()
                        .enumerate()
                        .map(|(i, a)| a * sign(p.seed, rep, j, i))
                        .sum();
                    out.push((s * scale) as f32);
                }
            }
        }
    }
    out
}

/// The encoding of a **document's** vectors: per bucket the mean, an empty bucket its nearest
/// vector. Empty for no vectors. Vectors of another length than the first's are ignored.
#[must_use]
pub fn encode_doc(p: &Params, vectors: &[Vec<f32>]) -> Vec<f32> {
    encode(p, vectors, true)
}

/// The encoding of a **query's** vectors: per bucket the sum, an empty bucket zero.
#[must_use]
pub fn encode_query(p: &Params, vectors: &[Vec<f32>]) -> Vec<f32> {
    encode(p, vectors, false)
}
