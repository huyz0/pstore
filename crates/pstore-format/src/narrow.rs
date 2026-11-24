//! f16 and i8 storage of a dense field's full precision vectors (M289).
//!
//! What a segment's `Vectors` sections hold when an index's `vector_storage` is narrower than
//! f32. Each vector is its first `dims - tail` components in the narrow element type, then
//! its last `tail` components as little-endian f32: the metric's own extra component (M9d's
//! euclidean `-|x|^2 / 2`) is a different magnitude from the rest and would cost the others
//! their precision if it shared their scale.
//!
//! - **f16**: each component rounded to the nearest f16, ties to even (M178's conversion).
//! - **i8**: one signed exponent byte `e`, then each component `q = round(x / 2^e)`, ties to
//!   even, read back as `q * 2^e`. `e` is the smallest in `-127..=127` with every `q` in
//!   `-128..=127`, made canonical (the decoded vector's own smallest). A power of two, so
//!   decoding is exact and re-encoding a decoded vector writes the same bytes; integers from
//!   -128 to 127 come back exact.
//!
//! Centroids, RaBitQ and int8 codes are not stored here, and stay as they were.

/// What a dense field's vector components are stored as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Elem {
    /// Little-endian f32, four bytes a component: every segment before M289.
    #[default]
    F32,
    /// Little-endian f16, two bytes a component.
    F16,
    /// A power-of-two exponent a vector, then one signed byte a component.
    I8,
}

impl Elem {
    /// The `kind` code a field of this element type has in a segment's Fields table: 0 for
    /// f32 (as every segment before M289 wrote), 2 for f16, 3 for i8. 1 is sparse.
    #[must_use]
    pub fn kind(self) -> u8 {
        match self {
            Self::F32 => 0,
            Self::F16 => 2,
            Self::I8 => 3,
        }
    }

    /// The element type a dense field's `kind` code names, or `None` for any other code.
    #[must_use]
    pub fn of_kind(kind: u8) -> Option<Self> {
        match kind {
            0 => Some(Self::F32),
            2 => Some(Self::F16),
            3 => Some(Self::I8),
            _ => None,
        }
    }

    /// Its name, as a setting gives it: `f32`, `f16` or `i8`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F16 => "f16",
            Self::I8 => "i8",
        }
    }

    /// The element type a setting names, or `None` for any other name.
    #[must_use]
    pub fn of_name(name: &str) -> Option<Self> {
        match name {
            "f32" => Some(Self::F32),
            "f16" => Some(Self::F16),
            "i8" => Some(Self::I8),
            _ => None,
        }
    }
}

/// What a sealed segment's dense vectors are stored as (M289): the index's `vector_storage`
/// over the cluster's default, and how many trailing components of each vector stay f32.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Vectors {
    /// The narrow element type, or f32 for what every segment before M289 holds.
    pub elem: Elem,
    /// How many of each vector's last components stay f32: the metric's extra components
    /// (1 under euclidean, else 0). Set by the engine from the index's metric; not a setting.
    pub tail: u8,
}

/// The bytes one vector of `dims` components takes, `tail` of them f32.
#[must_use]
pub fn vector_len(elem: Elem, dims: usize, tail: usize) -> usize {
    let head = dims.saturating_sub(tail);
    let tail = dims - head;
    match elem {
        Elem::F32 => dims * 4,
        Elem::F16 => head * 2 + tail * 4,
        Elem::I8 => 1 + head + tail * 4,
    }
}

/// Whether every component but the last `tail` of every vector is finite as an f16: what an
/// f16 field must hold, or the field is stored f32 in that segment, whole.
#[must_use]
pub fn fits_f16<'a>(vectors: impl IntoIterator<Item = &'a [f32]>, tail: usize) -> bool {
    vectors.into_iter().all(|v| {
        let head = v.len().saturating_sub(tail);
        v.iter()
            .take(head)
            .all(|x| crate::sparse::f32_to_f16(*x) & 0x7c00 != 0x7c00)
    })
}

/// Whether every component but the last `tail` of every vector is at most `2^127` in
/// magnitude: what an i8 field must hold, or the field is stored f32 in that segment, whole.
/// Above it the exponent reaches 122, where a code of 64 decodes to `2^128`, infinite in f32.
#[must_use]
pub fn fits_i8<'a>(vectors: impl IntoIterator<Item = &'a [f32]>, tail: usize) -> bool {
    let bound = f32::from_bits(0x7f00_0000); // 2^127
    vectors.into_iter().all(|v| {
        let head = v.len().saturating_sub(tail);
        v.iter().take(head).all(|x| x.abs() <= bound)
    })
}

/// Appends `v`, of exactly `dims` components (missing ones as 0), in `elem`.
pub fn encode(elem: Elem, v: &[f32], dims: usize, tail: usize, out: &mut Vec<u8>) {
    let at = |j: usize| v.get(j).copied().unwrap_or(0.0);
    let head = dims.saturating_sub(tail);
    match elem {
        Elem::F32 => {
            for j in 0..dims {
                out.extend_from_slice(&at(j).to_le_bytes());
            }
            return;
        }
        Elem::F16 => {
            for j in 0..head {
                out.extend_from_slice(&crate::sparse::f32_to_f16(at(j)).to_le_bytes());
            }
        }
        Elem::I8 => {
            let comps: Vec<f32> = (0..head).map(at).collect();
            let (e, codes) = canonical(&comps, exponent(&comps));
            out.extend_from_slice(&e.to_le_bytes());
            for q in codes {
                out.extend_from_slice(&q.to_le_bytes());
            }
        }
    }
    for j in head..dims {
        out.extend_from_slice(&at(j).to_le_bytes());
    }
}

/// One vector of `dims` components back from `raw`, exactly [`vector_len`] bytes; `None`
/// when `raw` is another length or an i8 exponent is outside `-127..=127`.
#[must_use]
pub fn decode(elem: Elem, raw: &[u8], dims: usize, tail: usize) -> Option<Vec<f32>> {
    if raw.len() != vector_len(elem, dims, tail) {
        return None;
    }
    let head = dims.saturating_sub(tail);
    let mut out = Vec::with_capacity(dims);
    let rest = match elem {
        Elem::F32 => raw,
        Elem::F16 => {
            let (body, rest) = raw.split_at(head * 2);
            out.extend(body.chunks_exact(2).map(|b| {
                crate::sparse::f16_to_f32(u16::from_le_bytes(b.try_into().unwrap_or([0; 2])))
            }));
            rest
        }
        Elem::I8 => {
            let (e, rest) = raw.split_first()?;
            let e = i8::from_le_bytes([*e]);
            if e == i8::MIN {
                return None;
            }
            let (body, rest) = rest.split_at(head);
            let scale = pow2(e);
            out.extend(
                body.iter()
                    .map(|q| dequantize(i8::from_le_bytes([*q]), scale)),
            );
            rest
        }
    };
    out.extend(
        rest.chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap_or([0; 4]))),
    );
    Some(out)
}

/// `2^e` as an f64, exactly: `e` in `-127..=127` is a normal f64.
fn pow2(e: i8) -> f64 {
    f64::from_bits(((1023 + i64::from(e)) as u64) << 52)
}

/// `q * scale` as an f32. Exact: `|q| <= 128` and `scale = 2^e` with `e >= -127` make a
/// multiple of f32's smallest subnormal step, `2^-149`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "exact by the bound above, or infinite only past f32's range"
)]
fn dequantize(q: i8, scale: f64) -> f32 {
    (f64::from(q) * scale) as f32
}

/// `x / 2^e`, rounded to the nearest integer, ties to even, as an f64 (exact: the division
/// by a power of two is).
fn scaled(x: f32, e: i8) -> f64 {
    (f64::from(x) / pow2(e)).round_ties_even()
}

/// Whether every component's `q` under `e` is in `-128..=127`.
fn fits(v: &[f32], e: i8) -> bool {
    v.iter().all(|x| (-128.0..=127.0).contains(&scaled(*x, e)))
}

/// The smallest exponent in `-127..=127` every component fits under. The guess from the
/// largest magnitude `m`, `ceil(log2(m / 127))`, always fits (`m / 2^guess <= 127`; below
/// `127 * 2^-127`, and for a zero vector, whose `log2` is minus infinity, the clamp to -127
/// is smaller still), and `m / 2^(guess - 2) > 254` never does, so only the next exponent
/// down can also fit, where a component rounds to 127 or the least is -128. `fits` is
/// monotone in `e`; the second step down is tried only as a margin for `log2`'s rounding.
/// (`fits_i8` has refused a component that is not finite or past `2^127`.)
#[expect(
    clippy::cast_possible_truncation,
    reason = "the guess is clamped to -127..=127 first"
)]
pub(crate) fn exponent(v: &[f32]) -> i8 {
    let m = v.iter().map(|x| f64::from(x.abs())).fold(0.0f64, f64::max);
    let guess = (m / 127.0).log2().ceil().clamp(-127.0, 127.0) as i8;
    let mut e = guess;
    for c in (-127..guess).rev().take(2) {
        if !fits(v, c) {
            break;
        }
        e = c;
    }
    e
}

/// The codes of `v` under `e`, then, while every code is in `-64..=63`, the same values at
/// the next smaller exponent: twice the codes. The decoded vector is unchanged, and its own
/// smallest exponent is the one written, so re-encoding it writes the same bytes. (A vector
/// whose least component rounded to -64 at `e` may not fit at `e - 1`, while its decoded
/// value, -128 there, does: i8's range is one wider below.)
fn canonical(v: &[f32], e: i8) -> (i8, Vec<i8>) {
    let mut codes: Vec<i8> = v.iter().map(|x| quantize(*x, e)).collect();
    let mut e = e;
    while e > -127 && codes.iter().all(|q| (-64..=63).contains(q)) {
        for q in &mut codes {
            *q *= 2;
        }
        e -= 1;
    }
    (e, codes)
}

/// `x`'s code under `e`, which [`exponent`] chose so it fits.
#[expect(
    clippy::cast_possible_truncation,
    reason = "clamped to i8's range, which `exponent` already guarantees"
)]
fn quantize(x: f32, e: i8) -> i8 {
    scaled(x, e).clamp(-128.0, 127.0) as i8
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "tests: bit-exact comparisons are the point"
)]
mod tests {
    use super::*;

    fn round(elem: Elem, v: &[f32], tail: usize) -> Vec<f32> {
        let mut out = Vec::new();
        encode(elem, v, v.len(), tail, &mut out);
        assert_eq!(out.len(), vector_len(elem, v.len(), tail));
        decode(elem, &out, v.len(), tail).unwrap()
    }

    #[test]
    fn lengths_are_per_element_type() {
        assert_eq!(vector_len(Elem::F32, 8, 1), 32);
        assert_eq!(vector_len(Elem::F16, 8, 0), 16);
        assert_eq!(vector_len(Elem::F16, 8, 1), 18);
        assert_eq!(vector_len(Elem::I8, 8, 0), 9);
        assert_eq!(vector_len(Elem::I8, 8, 1), 12);
        // A tail longer than the vector is the whole vector, f32.
        assert_eq!(vector_len(Elem::I8, 2, 5), 1 + 8);
    }

    #[test]
    fn codes_and_names_round_trip() {
        for e in [Elem::F32, Elem::F16, Elem::I8] {
            assert_eq!(Elem::of_kind(e.kind()), Some(e));
            assert_eq!(Elem::of_name(e.name()), Some(e));
        }
        assert_eq!(Elem::of_kind(1), None);
        assert_eq!(Elem::of_kind(4), None);
        assert_eq!(Elem::of_name("bf16"), None);
    }

    #[test]
    fn integers_are_exact_under_i8_and_pick_the_smallest_exponent() {
        let v = [-128.0, 127.0, 0.0, 3.0, -1.0];
        assert_eq!(round(Elem::I8, &v, 0), v);
        assert_eq!(exponent(&v), 0);
        // Small integers take a finer exponent and still come back exact.
        assert_eq!(exponent(&[1.0, -3.0]), -5);
        assert_eq!(round(Elem::I8, &[1.0, -3.0], 0), [1.0, -3.0]);
        // 128 does not fit at 0; -128 does.
        assert_eq!(exponent(&[128.0]), 1);
        assert_eq!(exponent(&[-128.0]), 0);
        assert_eq!(exponent(&[0.0, 0.0]), -127);
        assert_eq!(round(Elem::I8, &[0.0, 0.0], 0), [0.0, 0.0]);
    }

    #[test]
    fn i8_rounds_ties_to_even_within_half_a_step() {
        // At e = 0, 2.5 -> 2 and 3.5 -> 4; 100 keeps e at 0.
        assert_eq!(round(Elem::I8, &[2.5, 3.5, 100.0], 0), [2.0, 4.0, 100.0]);
        let mut seed = 7u64;
        for scale in [1e-30f32, 1e-5, 1.0, 3e4, 1e30] {
            for _ in 0..200 {
                let v: Vec<f32> = (0..12)
                    .map(|_| {
                        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                        ((seed >> 33) as f32 / (1u64 << 31) as f32 - 0.5) * scale
                    })
                    .collect();
                let e = exponent(&v);
                let back = round(Elem::I8, &v, 0);
                let step = pow2(e);
                for (x, y) in v.iter().zip(&back) {
                    assert!(
                        (f64::from(*x) - f64::from(*y)).abs() <= step / 2.0,
                        "{x} came back {y} at e = {e}"
                    );
                }
                // Idempotent: a decoded vector encodes to the same bytes.
                let (mut a, mut b) = (Vec::new(), Vec::new());
                encode(Elem::I8, &v, 12, 0, &mut a);
                encode(Elem::I8, &back, 12, 0, &mut b);
                assert_eq!(a, b);
            }
        }
    }

    #[test]
    fn f16_rounds_ties_to_even_and_keeps_its_values() {
        // 1 + 2^-11 is halfway between 1 and 1 + 2^-10: to even, 1.
        let tie = 1.0 + 2f32.powi(-11);
        let above = 1.0 + 3.0 * 2f32.powi(-11);
        assert_eq!(
            round(Elem::F16, &[tie, above, 65504.0, -0.5], 0),
            [1.0, 1.0 + 2.0 * 2f32.powi(-10), 65504.0, -0.5]
        );
        assert!(fits_f16([&[65504.0f32, 65519.0][..]], 0));
        assert!(!fits_f16([&[65520.0f32][..]], 0));
        assert!(!fits_f16([&[1.0f32][..], &[-1e9][..]], 0));
        // A tail is not judged: it stays f32.
        assert!(fits_f16([&[1.0f32, -1e9][..]], 1));
    }

    #[test]
    fn i8_holds_up_to_two_to_the_127_and_decodes_it_finite() {
        let top = f32::from_bits(0x7f00_0000);
        for x in [top, -top] {
            assert!(fits_i8([&[x, 1.0][..]], 0));
            let back = round(Elem::I8, &[x, 1.0], 0);
            assert!(back.iter().all(|v| v.is_finite()), "{x} came back {back:?}");
            assert_eq!(back[0], x);
        }
        let over = f32::from_bits(0x7f00_0001);
        assert!(!fits_i8([&[1.0f32][..], &[-over][..]], 0));
        assert!(!fits_i8([&[f32::MAX][..]], 0));
        // A tail is not judged: it stays f32.
        assert!(fits_i8([&[1.0f32, f32::MAX][..]], 1));
    }

    #[test]
    fn the_tail_stays_f32_bit_for_bit() {
        let t = -123_456.79f32;
        for elem in [Elem::F16, Elem::I8] {
            let back = round(elem, &[0.25, -0.5, t], 1);
            assert_eq!(back[..2], [0.25, -0.5]);
            assert_eq!(back[2].to_bits(), t.to_bits());
        }
    }

    #[test]
    fn a_wrong_length_or_exponent_is_refused() {
        let mut raw = Vec::new();
        encode(Elem::I8, &[1.0, 2.0], 2, 0, &mut raw);
        assert!(decode(Elem::I8, &raw[..2], 2, 0).is_none());
        assert!(decode(Elem::F16, &raw, 2, 0).is_none());
        raw[0] = 0x80;
        assert!(decode(Elem::I8, &raw, 2, 0).is_none());
        raw[0] = 127;
        assert!(decode(Elem::I8, &raw, 2, 0).is_some());
        raw[0] = 0x81; // -127
        assert!(decode(Elem::I8, &raw, 2, 0).is_some());
    }
}
