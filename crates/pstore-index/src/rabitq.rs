//! RaBitQ: 1-bit quantization with an error bound and **no training** (D-11).
//!
//! ## Why this and not PQ
//!
//! PQ needs k-means over a per-index sample. At millions of tenants that is codebooks to
//! schedule, store, version and re-run as distributions drift — and a cold ten-document
//! index has no sample to train on. RaBitQ's construction is data-independent: one
//! rotation derived from the dimension alone, working identically on ten vectors and ten
//! billion. For a multi-tenant product that single property outweighs accuracy
//! comparisons. PQ also has *no* error bound and reaches ~100% relative error on some real
//! data, which for arbitrary customer embeddings is a defect rather than a tradeoff.
//!
//! ## The construction
//!
//! A unit vector `x` is rotated by a fixed random orthogonal transform, then each
//! coordinate is kept only as its sign. The quantized vector is
//! `x̄ = sign(Px) / sqrt(D)`, one bit per dimension.
//!
//! The naive estimate `<x̄, q>` is **biased** — `x̄` is not `x`, and the angle between them
//! is systematic rather than random. RaBitQ's insight is that the bias is *measurable at
//! encode time*: `<x̄, x>` is a scalar the encoder computes and stores, and dividing by it
//! removes the bias. That single stored float is the difference between a bound and a hope,
//! which is why [`Quantizer::estimate_unnormalized_for_test`] exists to show what happens
//! without it.
//!
//! ## ⚠️ Residuals, not raw vectors — and why recall collapses without them
//!
//! A code is built from `(o - c) / ||o - c||`, the **residual** from the posting list's
//! centroid, with `||o - c||` stored alongside. `<o, q>` is then reconstructed as
//! `<c, q> + ||o - c|| · <x, q>`, where `<c, q>` is computed once per probed list.
//!
//! Encoding the raw vector instead is not a small loss, it is the difference between
//! working and not. Vectors in one posting list are similar *by construction* — that is
//! what put them in the same list — so their inner products with a query differ in the
//! third decimal place while the 1-bit estimator's error is two orders of magnitude larger.
//! The codes rank them essentially at random. Measured: recall@10 of **0.365** at p=16, and
//! still only **0.528** with every list probed, on a corpus where exhaustive rerank of the
//! candidates should have given ~1.0. Subtracting the centroid removes the component every
//! member shares and leaves the quantizer the part that actually distinguishes them.
//!
//! Found by the recall test, not by reading the paper.
//!
//! ## The rotation
//!
//! A Randomized Hadamard Transform: random sign flips, then a fast Walsh–Hadamard
//! transform. `O(D log D)`, **no matrix stored**, and derived from the dimension and a
//! global constant — never from the data, which would be training by another name.
//!
//! ⚠️ The transform needs a power-of-two length, so a vector is zero-padded first — and the
//! code covers the **padded** length, not the original dimension. The rotation mixes energy
//! into the padding, so those coordinates are not zero afterwards; storing signs for only
//! the first `D` of them and inventing the rest makes the `<x̄, x>` measured at encode time
//! describe a different `x̄` than the one the estimate uses, and the bound then fails about
//! 10% of the time. That is not a subtle inefficiency, it is a wrong estimator, and it was
//! found by the bound test rather than by reading this code. The cost is real: 384
//! dimensions occupy 512 bits, so a dimension near a power of two is materially cheaper.

use core::fmt;

/// The bound's confidence parameter.
///
/// The error term is a fixed vector's projection onto a random direction, which is
/// sub-Gaussian with parameter `1/sqrt(D-1)`. Two-sided, that gives a failure probability
/// of at most `2·exp(-ε²/2)`; for the δ = 1e-3 the M3 spec pins, `ε = sqrt(2·ln(2000)) ≈
/// 3.90`.
///
/// ⚠️ Derived, not tuned. The first draft used 2.25 on a half-remembered constant and the
/// measured failure rate came out at 2.6% — which is exactly what a 2.25σ two-sided bound
/// should do. The test did not catch a bug in the estimator; it caught a bound that had
/// never been derived. Raising ε makes the bound hold more often and say less, so the
/// number is written with its arithmetic beside it.
pub const BOUND_EPSILON: f32 = 3.90;

/// Fixed, global, and not a secret — it must be identical in every process that ever reads
/// a code, so it is a constant rather than configuration.
const ROTATION_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// What can go wrong, which is only ever a shape mismatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuantError {
    /// The vector's length is not the dimension this quantizer was built for.
    Dimension {
        /// What the quantizer expects.
        expected: usize,
        /// What it was given.
        got: usize,
    },
}

impl fmt::Display for QuantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dimension { expected, got } => {
                write!(f, "expected {expected} dimensions, got {got}")
            }
        }
    }
}

impl core::error::Error for QuantError {}

/// One vector's 1-bit code, with the scalar that makes it unbiased.
#[derive(Debug, Clone, PartialEq)]
pub struct Code {
    bits: Vec<u8>,
    /// `<x̄, x>` — how well the sign vector represents the original.
    ///
    /// Near `sqrt(2/π) ≈ 0.798` for a random unit vector in high dimension, and *higher*
    /// for a vector that happens to align with the rotated axes. Stored rather than
    /// assumed, because it varies per vector and it is what the bound is a function of.
    alignment: f32,
    /// `||o - c||`, the distance from the centroid this code's residual was taken against.
    ///
    /// Without it the estimate is of the residual's direction only, and the reconstruction
    /// `<c,q> + norm·<x,q>` has no scale.
    residual_norm: f32,
    dim: usize,
}

impl Code {
    /// The packed sign bits, one per **padded** dimension.
    ///
    /// `padded` is the dimension rounded up to a power of two — see the module docs for why
    /// it is not `dim`, and what it costs.
    #[must_use]
    pub fn bits(&self) -> &[u8] {
        &self.bits
    }

    /// `<x̄, x>`, the alignment between the code and the vector it came from.
    #[must_use]
    pub fn alignment(&self) -> f32 {
        self.alignment
    }

    /// `||o - c||`.
    #[must_use]
    pub fn residual_norm(&self) -> f32 {
        self.residual_norm
    }

    /// Appends this code's wire form: sign bits, then the alignment.
    ///
    /// ⚠️ The alignment travels **with** the bits. It is measured per vector at encode time
    /// and the estimate is meaningless without it, so a section holding bits alone would be
    /// a section of unusable codes — and would look fine, because the bits would decode.
    /// Four bytes a vector, which is why the 1-bit tier is 22x smaller than float32 rather
    /// than the 24x the bit count alone suggests.
    pub fn write_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.bits);
        out.extend_from_slice(&self.alignment.to_le_bytes());
        out.extend_from_slice(&self.residual_norm.to_le_bytes());
    }
}

/// A query, rotated once and ready to score against many codes.
#[derive(Debug, Clone)]
pub struct Query(Vec<f32>);

/// A dimension's quantizer. Holds no data-derived state — that is the point.
#[derive(Debug, Clone)]
pub struct Quantizer {
    /// Whether the Randomized Hadamard Transform is applied.
    ///
    /// ⚠️ Off is **not** a supported configuration — it exists so OQ-39 can measure what the
    /// rotation buys, which is the substantive difference between RaBitQ and BBQ. BBQ
    /// (Lucene) subtracts a centroid and quantizes the residual's signs with per-vector
    /// corrections, and does *not* rotate; RaBitQ's error bound is derived over the
    /// randomized transform and does not hold without it.
    rotate: bool,
    dim: usize,
    /// Padded to a power of two, which the Walsh–Hadamard transform requires.
    padded: usize,
    /// One sign flip per padded coordinate, derived from the dimension alone.
    flips: Vec<f32>,
}

impl Quantizer {
    /// A quantizer for `dim`-dimensional vectors.
    #[must_use]
    pub fn new(dim: usize) -> Self {
        let padded = dim.max(1).next_power_of_two();
        // Derived from the dimension and a fixed constant. Two quantizers built anywhere,
        // at any time, for the same dimension are byte-identical -- which is the
        // no-training property expressed as code rather than as a claim.
        let mut state = ROTATION_SEED ^ (padded as u64);
        let flips = (0..padded)
            .map(|_| {
                state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = state;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                if (z ^ (z >> 31)) & 1 == 0 { 1.0 } else { -1.0 }
            })
            .collect();
        Self {
            rotate: true,
            dim,
            padded,
            flips,
        }
    }

    /// A quantizer with the rotation disabled, for the OQ-39 comparison.
    ///
    /// ⚠️ Not for production. Without the randomized transform the error bound has no
    /// derivation behind it, and structured input — which real embeddings are — collapses
    /// the code's information content.
    #[doc(hidden)]
    #[must_use]
    pub fn without_rotation(dim: usize) -> Self {
        Self {
            rotate: false,
            ..Self::new(dim)
        }
    }

    /// Quantizes a prepared query to int4, BBQ-style.
    ///
    /// The asymmetry BBQ contributes: 1-bit documents scored against a low-precision query
    /// costs nothing in storage and buys integer SIMD. Measured here because our query is
    /// currently full `f32`, which is *more* accurate than int4 — so this is a throughput
    /// trade, not an accuracy one, and OQ-39 should say what it costs.
    #[doc(hidden)]
    #[must_use]
    pub fn quantize_query_int4(&self, q: &Query) -> Query {
        let hi = q.0.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        if hi <= 0.0 {
            return q.clone();
        }
        let step = hi / 7.0;
        Query(q.0.iter().map(|x| (x / step).round() * step).collect())
    }

    /// Bytes one code occupies on the wire: sign bits plus the alignment.
    #[must_use]
    pub fn code_len(&self) -> usize {
        self.padded.div_ceil(8) + 8
    }

    /// Reads a code back from [`Code::write_to`]'s form.
    ///
    /// Returns `None` for a wrong-length record rather than decoding a shifted one: a
    /// fixed-width section read at the wrong stride yields plausible bits and silently wrong
    /// answers, which is the failure this shape exists to make impossible.
    #[must_use]
    pub fn read_code(&self, raw: &[u8]) -> Option<Code> {
        if raw.len() != self.code_len() {
            return None;
        }
        let split = self.padded.div_ceil(8);
        let (bits, tail) = raw.split_at_checked(split)?;
        Some(Code {
            bits: bits.to_vec(),
            alignment: f32::from_le_bytes(tail.get(..4)?.try_into().ok()?),
            residual_norm: f32::from_le_bytes(tail.get(4..8)?.try_into().ok()?),
            dim: self.dim,
        })
    }

    /// The dimension this quantizer encodes.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.dim
    }

    fn check(&self, v: &[f32]) -> Result<(), QuantError> {
        if v.len() == self.dim {
            Ok(())
        } else {
            Err(QuantError::Dimension {
                expected: self.dim,
                got: v.len(),
            })
        }
    }

    /// Pads, flips signs, and applies the Walsh–Hadamard transform.
    ///
    /// Orthogonal up to the `1/sqrt(padded)` scaling applied at the end, so inner products
    /// are preserved — which is what lets the estimate be compared against the original
    /// vectors' inner product at all.
    fn rotate(&self, v: &[f32]) -> Vec<f32> {
        if !self.rotate {
            let mut a = vec![0.0f32; self.padded];
            for (slot, x) in a.iter_mut().zip(v) {
                *slot = *x;
            }
            return a;
        }
        let mut a = vec![0.0f32; self.padded];
        for ((slot, x), flip) in a.iter_mut().zip(v).zip(&self.flips) {
            *slot = x * flip;
        }
        let mut h = 1;
        while h < self.padded {
            for chunk in a.chunks_mut(h * 2) {
                let (lo, hi) = chunk.split_at_mut(h);
                for (x, y) in lo.iter_mut().zip(hi.iter_mut()) {
                    let (u, w) = (*x, *y);
                    *x = u + w;
                    *y = u - w;
                }
            }
            h *= 2;
        }
        let scale = 1.0 / (self.padded as f32).sqrt();
        for x in a.iter_mut() {
            *x *= scale;
        }
        a
    }

    /// Encodes a vector as its residual from `centroid`.
    ///
    /// See the module docs: quantizing the raw vector instead costs most of the recall,
    /// because everything in one posting list is similar and the shared component is all
    /// the 1-bit code can see.
    pub fn encode_residual(&self, v: &[f32], centroid: &[f32]) -> Result<Code, QuantError> {
        self.check(v)?;
        let residual: Vec<f32> = v
            .iter()
            .enumerate()
            .map(|(i, x)| x - centroid.get(i).copied().unwrap_or(0.0))
            .collect();
        let norm = residual.iter().map(|x| x * x).sum::<f32>().sqrt();
        // A vector sitting exactly on its centroid has no direction to quantize. Its
        // reconstruction is `<c,q>` with a zero scale, which is exactly right.
        let unit: Vec<f32> = if norm > 0.0 {
            residual.iter().map(|x| x / norm).collect()
        } else {
            residual
        };
        let mut code = self.encode_unit(&unit)?;
        code.residual_norm = norm;
        Ok(code)
    }

    /// Encodes a **unit** vector, with no centroid.
    pub fn encode(&self, v: &[f32]) -> Result<Code, QuantError> {
        self.encode_unit(v)
    }

    fn encode_unit(&self, v: &[f32]) -> Result<Code, QuantError> {
        self.check(v)?;
        let r = self.rotate(v);
        let mut bits = vec![0u8; self.padded.div_ceil(8)];
        // `<x̄, x>` where `x̄ = sign(x)/sqrt(padded)`, which is `||x||₁ / sqrt(padded)`.
        // Computed from the rotated vector, over the padded length, because that is the
        // space the estimate lives in.
        let l1: f32 = r.iter().map(|x| x.abs()).sum();
        for (i, x) in r.iter().enumerate() {
            if *x > 0.0
                && let Some(byte) = bits.get_mut(i / 8)
            {
                *byte |= 1 << (i % 8);
            }
        }
        Ok(Code {
            bits,
            alignment: l1 / (self.padded as f32).sqrt(),
            // Overwritten by `encode_residual`; 1.0 means "the vector is its own residual",
            // which is what makes `encode` and `encode_residual` agree for a zero centroid.
            residual_norm: 1.0,
            dim: self.dim,
        })
    }

    /// `<x̄, q>` — the raw, **biased** inner product of the code with a rotated query.
    fn raw(&self, code: &Code, rotated_query: &[f32]) -> f32 {
        let scale = 1.0 / (self.padded as f32).sqrt();
        let mut acc = 0.0f32;
        for (i, q) in rotated_query.iter().enumerate() {
            let bit = code
                .bits
                .get(i / 8)
                .is_some_and(|b| b & (1 << (i % 8)) != 0);
            acc += if bit { *q } else { -*q };
        }
        acc * scale
    }

    /// Rotates a query once, for scoring against many codes.
    ///
    /// ⚠️ The asymmetry D-11 buys accuracy from: the query is transformed **once per
    /// query**, not once per vector, so work spent on it is amortised across every code in
    /// every probed posting list. Rotating inside `estimate` would put an `O(D log D)`
    /// transform on the inner loop of a scan over thousands of vectors.
    pub fn prepare(&self, query: &[f32]) -> Result<Query, QuantError> {
        self.check(query)?;
        Ok(Query(self.rotate(query)))
    }

    /// Estimates `<o, q>` where the code is a residual from a centroid.
    ///
    /// `centroid_dot` is `<c, q>`, computed **once per probed posting list** rather than
    /// once per candidate — there are thousands of candidates behind each centroid, and
    /// that ratio is the whole reason this is cheap.
    #[must_use]
    pub fn estimate_residual(&self, code: &Code, query: &Query, centroid_dot: f32) -> f32 {
        centroid_dot + code.residual_norm * self.estimate_prepared(code, query)
    }

    /// Estimates `<o, q>` from `o`'s code and a prepared query.
    #[must_use]
    pub fn estimate_prepared(&self, code: &Code, query: &Query) -> f32 {
        // ⚠️ The division is the whole construction. `<x̄, q>` is biased low by exactly the
        // factor `<x̄, x>`, which the encoder measured; dividing removes it.
        self.raw(code, &query.0) / code.alignment
    }

    /// Estimates `<o, q>` from `o`'s code and a unit query.
    pub fn estimate(&self, code: &Code, query: &[f32]) -> Result<f32, QuantError> {
        Ok(self.estimate_prepared(code, &self.prepare(query)?))
    }

    /// The same estimate **without** the normalisation, so a test can show the bound is
    /// tight enough to reject it.
    #[doc(hidden)]
    pub fn estimate_unnormalized_for_test(&self, code: &Code, query: &Query) -> f32 {
        self.raw(code, &query.0)
    }

    /// The error bound for this code, at [`BOUND_EPSILON`].
    ///
    /// Widens as the code's alignment falls: a vector the sign pattern represents poorly is
    /// one the estimate is allowed to be more wrong about. That dependence is why the bound
    /// is a function of the code rather than a constant.
    #[must_use]
    pub fn error_bound(&self, code: &Code) -> f32 {
        let a = code.alignment.max(f32::EPSILON);
        let d = (self.padded.max(2) - 1) as f32;
        BOUND_EPSILON * ((1.0 - a * a).max(0.0) / d).sqrt() / a
    }
}
