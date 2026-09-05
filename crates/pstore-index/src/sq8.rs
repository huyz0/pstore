//! int8 scalar quantization — rung 1 of the rerank ladder.
//!
//! Four bytes a dimension down to one, with a per-vector scale and offset. Unlike RaBitQ
//! this is not clever and does not need to be: its job is to be *decisively* more accurate
//! than the 1-bit codes for a quarter of the bytes of float32, on a candidate set already
//! narrowed by two orders of magnitude.
//!
//! Per-vector rather than per-index scaling, for the same reason RaBitQ beats PQ: a global
//! scale is fitted state, and fitted state is per-tenant training with a friendlier name.

/// One vector's int8 code.
#[derive(Debug, Clone, PartialEq)]
pub struct Code {
    codes: Vec<u8>,
    /// The value the lowest code represents.
    offset: f32,
    /// Value per code step.
    step: f32,
}

impl Code {
    /// The packed codes, one byte per dimension.
    #[must_use]
    pub fn codes(&self) -> &[u8] {
        &self.codes
    }

    /// The reconstruction step: no coordinate is further than this from its original.
    #[must_use]
    pub fn step(&self) -> f32 {
        self.step
    }
}

/// Quantizes a vector to one byte per dimension.
#[must_use]
pub fn encode(v: &[f32]) -> Code {
    let lo = v.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    // A constant vector has no range; a zero step reconstructs it exactly, and dividing by
    // it is the only thing that could go wrong, so it is handled here rather than guarded
    // at every use.
    let step = if hi > lo { (hi - lo) / 255.0 } else { 0.0 };
    let codes = v
        .iter()
        .map(|x| {
            if step > 0.0 {
                // ⚠️ `round`, not truncate. Truncation shifts every coordinate down by
                // about half a step, so the reconstructed vector is displaced by a constant
                // offset in every dimension at once rather than scattered around the
                // original. Against a zero-mean query that mostly cancels in the inner
                // product, which is why no accuracy test catches it -- only the
                // reconstruction bound does.
                (((x - lo) / step).round()).clamp(0.0, 255.0) as u8
            } else {
                0
            }
        })
        .collect();
    Code {
        codes,
        offset: if lo.is_finite() { lo } else { 0.0 },
        step,
    }
}

/// Reconstructs the approximate vector.
#[must_use]
pub fn decode(c: &Code) -> Vec<f32> {
    c.codes
        .iter()
        .map(|b| c.offset + f32::from(*b) * c.step)
        .collect()
}

/// Bytes one row occupies on the wire: the codes, then the offset and step.
#[must_use]
pub fn record_len(dim: usize) -> usize {
    dim + 8
}

/// Appends a code's wire form: codes, then `offset` and `step`.
///
/// ⚠️ The scale travels **with** the codes, and it has to. The quantization is per vector —
/// a global scale would be fitted state, which is per-tenant training under another name —
/// so two rows' codes are on different scales and comparing them raw is comparing
/// different units. Measured: reading the codes without their scale made `rerank: fast`
/// *worse* than no rerank at all (25 true neighbours against 26), while every depth and
/// byte assertion still passed.
pub fn write_to(c: &Code, out: &mut Vec<u8>) {
    out.extend_from_slice(&c.codes);
    out.extend_from_slice(&c.offset.to_le_bytes());
    out.extend_from_slice(&c.step.to_le_bytes());
}

/// Estimates `<o, q>` from one row's wire form.
#[must_use]
pub fn estimate_raw(record: &[u8], query: &[f32]) -> f32 {
    let Some(split) = record.len().checked_sub(8) else {
        return f32::NEG_INFINITY;
    };
    let (codes, tail) = record.split_at(split);
    let f = |at: usize| {
        tail.get(at..at + 4)
            .and_then(|b| b.try_into().ok())
            .map_or(0.0, f32::from_le_bytes)
    };
    let (offset, step) = (f(0), f(4));
    let mut sum_q = 0.0f32;
    let mut acc = 0.0f32;
    for (b, q) in codes.iter().zip(query) {
        sum_q += *q;
        acc += f32::from(*b) * *q;
    }
    offset * sum_q + step * acc
}

/// Estimates `<o, q>` from `o`'s code.
///
/// Expanded rather than reconstructing first: `<o,q> ≈ offset·Σq + step·Σ(c·q)`, which is
/// one pass and no allocation on a path that runs once per candidate.
#[must_use]
pub fn estimate(c: &Code, query: &[f32]) -> f32 {
    let mut sum_q = 0.0f32;
    let mut acc = 0.0f32;
    for (b, q) in c.codes.iter().zip(query) {
        sum_q += *q;
        acc += f32::from(*b) * *q;
    }
    c.offset * sum_q + c.step * acc
}
