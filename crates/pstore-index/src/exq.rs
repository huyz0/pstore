//! Extended RaBitQ: `B` bits a coordinate, as `B − 1` extra bits beside the 1-bit code
//! (M97, OQ-170).
//!
//! Gao et al., *Practical and Asymptotically Optimal Quantization of High-Dimensional
//! Vectors in Euclidean Space for ANN Search* (SIGMOD 2025). The question it is here to
//! answer is whether a rerank tier narrower than int8 holds int8's recall: at 384d an int8
//! record is 392 bytes and **84% of what a `fast` query moves** (`prism-comparison.md`).
//!
//! ## The construction
//!
//! The residual is the 1-bit code's: the same centroid, the same unit residual `o`, the
//! same rotation, so nothing new is trained. Each rotated coordinate is coded as
//! `y_i = sign(o'_i) · (m_i + 0.5)` with `m_i ∈ [0, 2^(B−1) − 1]`. The **sign is the 1-bit
//! code's bit**, so only the magnitudes `m_i` are new: `B − 1` bits a coordinate.
//!
//! `m_i = min(floor(t · |o'_i|), 2^(B−1) − 1)` for a per-vector scale `t`, and `t` is
//! chosen to maximise `<ȳ, o'>`, the cosine between the code and the residual. The cosine
//! only changes where some `t · |o'_i|` crosses an integer, so the search walks those
//! **critical values** in order, updating the cosine's numerator and denominator by one
//! coordinate each. At B=4 and 512 padded coordinates that is at most 3,584 steps a vector.
//!
//! The estimate is the 1-bit code's with `ȳ` in place of `x̄`: `<x, q> ≈ <ȳ, q'> / <ȳ, o'>`.
//! The division is the unbiasing, as `<x̄, x>` is for one bit, and the record stores both
//! `||y||` and `<ȳ, o'>` so that the estimator has no square root to take.
//!
//! ⚠️ The extra bits cover the **padded** dimension (`rabitq.rs` module docs): 384d codes
//! 512 coordinates, so a B=4 record is `3 · 512 / 8 + 8` = 200 bytes, not 152. M97's first
//! draft priced it over `D` and its review caught it.
//!
//! ⚠️ A measurement primitive, not a shipped tier. No section holds these records and no
//! search path reads them; wiring one in is the milestone after M97, if criterion 1 says so.

use core::fmt;

use crate::rabitq::{Code, QuantError, Quantizer, Query};

/// A bit width this module does not code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitsError(pub u32);

impl fmt::Display for BitsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "extended RaBitQ codes 2 or 4 bits a coordinate, not {}",
            self.0
        )
    }
}

impl core::error::Error for BitsError {}

/// One vector's extra bits: the magnitudes beside its 1-bit code's signs.
#[derive(Debug, Clone, PartialEq)]
pub struct ExCode {
    /// `B − 1` bit planes, plane `p` holding bit `p` of every coordinate's magnitude.
    planes: Vec<u8>,
    /// `||y||`, so the estimate is of the unit `ȳ`.
    norm: f32,
    /// `<ȳ, o'>`, the unbiasing divisor. 0 for a residual with no direction.
    cosine: f32,
}

impl ExCode {
    /// `<ȳ, o'>`: how well the code represents its residual's direction.
    #[must_use]
    pub fn cosine(&self) -> f32 {
        self.cosine
    }

    /// Appends the wire form: the bit planes, then `||y||` and `<ȳ, o'>`.
    pub fn write_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.planes);
        out.extend_from_slice(&self.norm.to_le_bytes());
        out.extend_from_slice(&self.cosine.to_le_bytes());
    }
}

/// A `B`-bit extended quantizer over a 1-bit [`Quantizer`]'s rotation.
#[derive(Debug, Clone)]
pub struct ExQuantizer {
    base: Quantizer,
    /// `B − 1`, the bits of a magnitude.
    extra: u32,
}

impl ExQuantizer {
    /// An extended quantizer at `bits` a coordinate, over `base`'s rotation.
    ///
    /// ⚠️ 2 and 4 only: the widths M97 measures. Another width is not wrong, but nothing
    /// says what it costs or buys, and a width that is accepted is a width someone ships.
    pub fn new(base: &Quantizer, bits: u32) -> Result<Self, BitsError> {
        if bits == 2 || bits == 4 {
            Ok(Self {
                base: base.clone(),
                extra: bits - 1,
            })
        } else {
            Err(BitsError(bits))
        }
    }

    fn plane_len(&self) -> usize {
        self.base.padded.div_ceil(8)
    }

    fn top(&self) -> u32 {
        (1 << self.extra) - 1
    }

    /// Bytes one record occupies: `(B − 1) · padded / 8` of planes, then two `f32`.
    #[must_use]
    pub fn record_len(&self) -> usize {
        self.extra as usize * self.plane_len() + 8
    }

    /// The rotated unit residual, exactly as the 1-bit code saw it.
    ///
    /// ⚠️ Taken by the 1-bit code's own `unit_residual`, so the signs here are the code's
    /// bits. Exposed so a test can search the critical values itself.
    #[doc(hidden)]
    pub fn rotated_residual(&self, v: &[f32], centroid: &[f32]) -> Result<Vec<f32>, QuantError> {
        let (unit, _) = self.base.unit_residual(v, centroid)?;
        Ok(self.base.rotate(&unit))
    }

    /// The cosine `<ȳ, o'>` of the code a fixed scale `t` gives, for the search test.
    #[doc(hidden)]
    #[must_use]
    pub fn cosine_at(&self, rotated: &[f32], t: f32) -> f32 {
        let top = self.top();
        let m: Vec<u32> = rotated
            .iter()
            .map(|x| ((t * x.abs()).floor() as u32).min(top))
            .collect();
        cosine(rotated, &m).1
    }

    /// Encodes `v`'s residual from `centroid`: the magnitudes beside its 1-bit code.
    pub fn encode_residual(&self, v: &[f32], centroid: &[f32]) -> Result<ExCode, QuantError> {
        let r = self.rotated_residual(v, centroid)?;
        let top = self.top();
        // Every critical value: coordinate `i`'s magnitude steps from `j - 1` to `j` at
        // `t = j / |o'_i|`. A zero coordinate never steps.
        let mut events: Vec<(f32, usize)> = r
            .iter()
            .enumerate()
            .filter(|(_, x)| **x != 0.0)
            .flat_map(|(i, x)| (1..=top).map(move |j| (j as f32 / x.abs(), i)))
            .collect();
        events.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        // From `t` just above 0, every magnitude 0: `<y, o'> = 0.5 Σ|o'|`, `||y||² = D/4`.
        let mut m = vec![0u32; r.len()];
        let mut num: f64 = r.iter().map(|x| 0.5 * f64::from(x.abs())).sum();
        let mut den2 = 0.25 * r.len() as f64;
        let mut best = (num / den2.sqrt(), 0usize);
        for (k, (_, i)) in events.iter().enumerate() {
            let (Some(mi), Some(x)) = (m.get_mut(*i), r.get(*i)) else {
                continue;
            };
            *mi += 1;
            num += f64::from(x.abs());
            // (m + 0.5)² - (m - 0.5)² = 2m, for the new m.
            den2 += 2.0 * f64::from(*mi);
            let c = num / den2.sqrt();
            if c > best.0 {
                best = (c, k + 1);
            }
        }
        // Replay the best prefix rather than re-flooring at its `t`: `floor(t · |o'_i|)`
        // at `t = j / |o'_i|` exactly can round to `j - 1`.
        m.fill(0);
        for (_, i) in events.iter().take(best.1) {
            if let Some(mi) = m.get_mut(*i) {
                *mi += 1;
            }
        }
        let (norm, cos) = cosine(&r, &m);
        let mut planes = vec![0u8; self.extra as usize * self.plane_len()];
        for (i, mi) in m.iter().enumerate() {
            for p in 0..self.extra as usize {
                if (mi >> p) & 1 == 1
                    && let Some(byte) = planes.get_mut(p * self.plane_len() + i / 8)
                {
                    *byte |= 1 << (i % 8);
                }
            }
        }
        Ok(ExCode {
            planes,
            norm,
            cosine: cos,
        })
    }

    /// Reads a record back from [`ExCode::write_to`]'s form; `None` at the wrong length.
    #[must_use]
    pub fn read(&self, raw: &[u8]) -> Option<ExCode> {
        if raw.len() != self.record_len() {
            return None;
        }
        let (planes, tail) = raw.split_at_checked(raw.len() - 8)?;
        Some(ExCode {
            planes: planes.to_vec(),
            norm: f32::from_le_bytes(tail.get(..4)?.try_into().ok()?),
            cosine: f32::from_le_bytes(tail.get(4..8)?.try_into().ok()?),
        })
    }

    /// Estimates `<o, q>` from the 1-bit code, its extra bits, and `<c, q>`.
    ///
    /// The same shape as `Quantizer::estimate_residual`: `<c, q> + ||o - c|| · <ȳ, q'> /
    /// <ȳ, o'>`, with the residual norm and the signs taken from `code`.
    #[must_use]
    pub fn estimate_residual(
        &self,
        code: &Code,
        extra: &ExCode,
        query: &Query,
        centroid_dot: f32,
    ) -> f32 {
        // No direction to estimate: a vector on its centroid, as the 1-bit code returns. A
        // zero residual is the only way `encode_residual` gives a zero cosine, so this one
        // test is both (and a `residual_norm() == 0.0` beside it was unkillable, M97).
        if extra.cosine == 0.0 {
            return centroid_dot;
        }
        let len = self.plane_len();
        let bits = code.bits();
        let mut acc = 0.0f32;
        for (i, q) in query.0.iter().enumerate() {
            let (byte, bit) = (i / 8, 1u8 << (i % 8));
            let mut mag = 0.5f32;
            for p in 0..self.extra as usize {
                if extra
                    .planes
                    .get(p * len + byte)
                    .is_some_and(|b| b & bit != 0)
                {
                    mag += (1u32 << p) as f32;
                }
            }
            let positive = bits.get(byte).is_some_and(|b| b & bit != 0);
            acc += if positive { mag * q } else { -mag * q };
        }
        centroid_dot + code.residual_norm() * acc / (extra.norm * extra.cosine)
    }
}

/// `(||y||, <ȳ, o'>)` for magnitudes `m` against the rotated residual `r`.
///
/// ⚠️ `r` is a **unit** vector or zero, as `rotated_residual` gives it: the rotation is
/// orthonormal, so `||o'||` is not divided by, as the search in `encode_residual` does not.
/// A `* ||o'||²` here was unkillable, and a search and a cosine that disagree about it would
/// pick a code by one measure and report it by another (M97).
fn cosine(r: &[f32], m: &[u32]) -> (f32, f32) {
    let mut num = 0.0f64;
    let mut y2 = 0.0f64;
    for (x, mi) in r.iter().zip(m) {
        let y = f64::from(*mi) + 0.5;
        num += f64::from(x.abs()) * y;
        y2 += y * y;
    }
    // `y2 >= D / 4 > 0`, so no branch: a zero residual has `num = 0` and so cosine 0, which
    // `estimate_residual` reads as "no direction".
    let cos = num / y2.sqrt();
    (y2.sqrt() as f32, cos as f32)
}
