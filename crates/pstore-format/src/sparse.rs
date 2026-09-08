//! Sparse postings: an inverted index keyed by dimension, with a generic impact payload.
//!
//! ⚠️ **This module owns the transposition and the quantization, and `pstore-format` owns
//! neither.** `writer.rs` states the rule — "the format does not quantize… the format owns
//! *where sections live*; whoever owns the codes hands them over as bytes" — so `build`
//! returns bytes and the caller attaches them with `SegmentWriter::with_section`. Doing the
//! inversion inside the writer would put the architecture in a comment instead of in
//! `Cargo.toml`.
//!
//! Two artefacts come out, and they live in different places for different reasons:
//!
//! | Artefact | Where | Why |
//! |---|---|---|
//! | Postings | [`Section::SparsePostings`](crate::Section) in the segment | Fetched by range; only the query's own terms cross the wire |
//! | Dictionary | a **sibling object**, `Class::Pinned` | `INDEX_BUDGET` is 8,150 bytes and a 30,000-term vocabulary is 44× that — and `try_finish` *refuses* an over-wide segment rather than opening it slowly, so the letter of the corpus's structures table makes the segment unwritable, not slow. **C-10** |

use crate::{Document, VectorField};

/// How an impact is stored (D-72, OQ-126).
///
/// ⚠️ The payload encoding is the configurable part *by design*: BM25 term frequencies,
/// SPLADE weights and miniCOIL weights are all impacts, and they do not want the same
/// precision. `pstore-format`'s [`Impact`](crate::Impact) keeps its representation
/// private for exactly this reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImpactEncoding {
    /// One byte, scaled per term by that term's largest magnitude. Error ≤ `max/254`.
    U8,
    /// Two bytes, IEEE binary16. No per-term scale needed.
    F16,
    /// Four bytes, exact.
    F32,
    /// A varint of the value rounded to an integer — **exact** for integers, and variable
    /// width.
    ///
    /// ⚠️ The third encoding D-72 names, and the one full-text needs: a term frequency is an
    /// integer, and `U8`'s per-term scale is a *quantization* — a tf of 3 in a list whose
    /// maximum is 4 decodes as 2.99, BM25 saturates it differently, and the scorer disagrees
    /// with an oracle for a reason no test names. Exact to 2^24, which is the largest integer
    /// an `f32` holds; a term appearing 16 million times in one document is not a case.
    Varint,
}

impl ImpactEncoding {
    /// Bytes one impact occupies, or **0 for variable**.
    #[must_use]
    pub fn width(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::F16 => 2,
            Self::F32 => 4,
            Self::Varint => 0,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::U8 => 1,
            Self::F16 => 2,
            Self::F32 => 4,
            Self::Varint => 5,
        }
    }

    fn from_tag(t: u8) -> Option<Self> {
        match t {
            1 => Some(Self::U8),
            2 => Some(Self::F16),
            4 => Some(Self::F32),
            5 => Some(Self::Varint),
            _ => None,
        }
    }
}

/// The encoding a fold writes unless told otherwise.
///
/// ⚠️ **u8 by decision, not by default-thinking** — D-72's cheapest option, and the one
/// M5a.4 measures against f16 and f32. If the measurement does not clear the floor the spec
/// pinned in advance, this constant is what changes.
pub const DEFAULT_ENCODING: ImpactEncoding = ImpactEncoding::U8;

/// The two byte strings a sparse field becomes.
#[derive(Debug, Clone, Default)]
pub struct Postings {
    /// The `SparsePostings` section: every term's list, back to back.
    pub section: Vec<u8>,
    /// The sibling dictionary object.
    pub dictionary: Vec<u8>,
}

/// Where one dimension's postings are, and what they are scaled by.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry {
    /// The dimension this list belongs to.
    pub dim: u32,
    /// Byte offset into the `SparsePostings` section.
    pub offset: u64,
    /// Bytes the list occupies — its exact fetch range.
    pub bytes: u32,
    /// Postings in the list.
    pub count: u32,
    /// The largest magnitude in the list, which `U8` scales against.
    pub max_impact: f32,
}

/// Where a segment's sparse dictionary lives, **derived** from the segment's own key.
///
/// ⚠️ Derived, never discovered. A compaction has to reach the sidecar of every segment it
/// merges — and a LIST to find it is priced like a PUT, caps at 1,000 keys, and would make
/// merging cost more as an index grows. The suffix is the whole mechanism.
#[must_use]
pub fn dict_key(segment: &pstore_blob::Key) -> pstore_blob::Key {
    pstore_blob::Key::new(format!("{}.sdict", segment.as_str()))
}

const MAGIC: &[u8; 8] = b"PSTORESD";
const VERSION: u16 = 1;
/// dim(4) offset(8) bytes(4) count(4) max(4).
const ENTRY: usize = 24;
const HEADER: usize = 8 + 2 + 1 + 4;

/// Builds the postings and the dictionary for one sparse field.
///
/// ⚠️ `docs` is taken in **segment row order**, not document order, and the postings address
/// segment rows. A caller that clusters its dense field reorders the rows; handing this the
/// unreordered slice produces a posting list that is internally consistent and points at the
/// wrong documents — which no round-trip of the list can see.
#[must_use]
pub fn build(docs: &[Document], field: &str, encoding: ImpactEncoding) -> Postings {
    // Dimension -> (row, weight), ascending in both by construction: rows are visited in
    // order and a BTreeMap orders the dimensions.
    let mut by_dim: std::collections::BTreeMap<u32, Vec<(u32, f32)>> =
        std::collections::BTreeMap::new();
    for (row, d) in docs.iter().enumerate() {
        let Some(VectorField::Sparse(pairs)) = d.vectors.get(field) else {
            continue;
        };
        for (dim, impact) in pairs {
            by_dim
                .entry(*dim)
                .or_default()
                .push((row as u32, impact.get()));
        }
    }

    let mut section: Vec<u8> = Vec::new();
    let mut entries: Vec<Entry> = Vec::with_capacity(by_dim.len());
    for (dim, list) in by_dim {
        let offset = section.len() as u64;
        let max_impact = list.iter().fold(0.0f32, |m, (_, w)| m.max(w.abs()));
        write_list(&mut section, &list, encoding, max_impact);
        entries.push(Entry {
            dim,
            offset,
            bytes: (section.len() as u64 - offset) as u32,
            count: list.len() as u32,
            max_impact,
        });
    }

    let mut dictionary = Vec::with_capacity(HEADER + entries.len() * ENTRY);
    dictionary.extend_from_slice(MAGIC);
    dictionary.extend_from_slice(&VERSION.to_le_bytes());
    dictionary.push(encoding.tag());
    dictionary.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in &entries {
        dictionary.extend_from_slice(&e.dim.to_le_bytes());
        dictionary.extend_from_slice(&e.offset.to_le_bytes());
        dictionary.extend_from_slice(&e.bytes.to_le_bytes());
        dictionary.extend_from_slice(&e.count.to_le_bytes());
        dictionary.extend_from_slice(&e.max_impact.to_le_bytes());
    }
    Postings {
        section,
        dictionary,
    }
}

/// Every row's `(dimension, impact)` pairs, reconstructed from a whole postings section.
///
/// ⚠️ The inverted index turned back the right way up, which is what a compaction needs and
/// **only** a compaction: it is `O(postings)` in time and bytes, against a query's
/// `O(the query's own lists)`. Used by `Segment::scan`, never by a retriever.
#[must_use]
pub fn transpose(dict: &Dictionary, section: &[u8], rows: usize) -> Vec<Vec<(u32, crate::Impact)>> {
    let mut out = vec![Vec::new(); rows];
    for i in 0..dict.len() {
        let Some(e) = dict.at(i) else { continue };
        let Some(raw) = section.get(e.offset as usize..e.offset as usize + e.bytes as usize) else {
            continue;
        };
        for (row, impact) in dict.decode_list(&e, raw) {
            if let Some(slot) = out.get_mut(row as usize) {
                slot.push((e.dim, crate::Impact::new(impact)));
            }
        }
    }
    // Ascending by dimension, which is how a document's pairs were written and what the
    // round-trip is compared against. Entries are visited in dimension order, so each row's
    // list is already sorted -- but a caller cannot see that, and a compaction that reorders
    // a document's pairs would make the merge's output differ from its input for no reason.
    for row in &mut out {
        row.sort_by_key(|(d, _)| *d);
    }
    out
}

/// The term dictionary: fixed-width entries, sorted by dimension.
///
/// ⚠️ Fixed width is what makes a lookup a binary search. A variable-width entry would force
/// a scan of the whole table per query term — 600 KB at 30,000 terms, per term, per query.
#[derive(Debug, Clone)]
pub struct Dictionary {
    raw: bytes::Bytes,
    encoding: ImpactEncoding,
    count: usize,
    /// The dimensions, ascending, decoded once.
    ///
    /// ⚠️ Four bytes a term of duplication, deliberately, so the lookup is
    /// `slice::binary_search` rather than a hand-written loop. Mutation testing found the
    /// reason: `lo < hi` flipped to `lo <= hi` does not return a wrong answer, it **fails to
    /// terminate** — on a query path — and the only test that could tell looks up a dimension
    /// falling in a *gap* between two entries, which is not an obvious case to write. Rung 1
    /// of the gate ladder: the condition that can be wrong no longer exists.
    dims: Vec<u32>,
}

impl Dictionary {
    /// Decodes a dictionary, returning `None` for anything malformed.
    #[must_use]
    pub fn decode(raw: &[u8]) -> Option<Self> {
        if raw.get(..8)? != MAGIC || u16::from_le_bytes(raw.get(8..10)?.try_into().ok()?) != VERSION
        {
            return None;
        }
        let encoding = ImpactEncoding::from_tag(*raw.get(10)?)?;
        let count = u32::from_le_bytes(raw.get(11..15)?.try_into().ok()?) as usize;
        // ⚠️ Length checked here, not per lookup. A truncated table decoded lazily would
        // answer some lookups and silently lose the terms past the cut.
        if raw.len() != HEADER + count * ENTRY {
            return None;
        }
        let mut me = Self {
            raw: bytes::Bytes::copy_from_slice(raw),
            encoding,
            count,
            dims: Vec::new(),
        };
        me.dims = (0..count).filter_map(|i| me.at(i).map(|e| e.dim)).collect();
        // A table that is not sorted cannot be searched, and searching it anyway answers
        // "absent" for terms that are present — a silent recall loss rather than a failure.
        if !me.dims.is_sorted_by(|a, b| a < b) {
            return None;
        }
        Some(me)
    }

    /// How impacts in this segment are stored.
    #[must_use]
    pub fn encoding(&self) -> ImpactEncoding {
        self.encoding
    }

    /// Distinct dimensions in the dictionary.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether the field has no postings at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn at(&self, i: usize) -> Option<Entry> {
        let b = self.raw.get(HEADER + i * ENTRY..HEADER + (i + 1) * ENTRY)?;
        Some(Entry {
            dim: u32::from_le_bytes(b.get(0..4)?.try_into().ok()?),
            offset: u64::from_le_bytes(b.get(4..12)?.try_into().ok()?),
            bytes: u32::from_le_bytes(b.get(12..16)?.try_into().ok()?),
            count: u32::from_le_bytes(b.get(16..20)?.try_into().ok()?),
            max_impact: f32::from_le_bytes(b.get(20..24)?.try_into().ok()?),
        })
    }

    /// Every dimension, ascending.
    pub fn dims(&self) -> impl Iterator<Item = u32> + '_ {
        self.dims.iter().copied()
    }

    /// One dimension's entry, or `None` when the vocabulary does not contain it.
    ///
    /// ⚠️ `None` is the **common** case, not an error: a query term the corpus never used
    /// contributes nothing, and must cost nothing — no range, no request, no failure.
    #[must_use]
    pub fn lookup(&self, dim: u32) -> Option<Entry> {
        self.dims.binary_search(&dim).ok().and_then(|i| self.at(i))
    }

    /// Decodes one posting list from exactly the bytes its entry addresses.
    #[must_use]
    pub fn decode_list(&self, entry: &Entry, raw: &[u8]) -> Vec<(u32, f32)> {
        read_list(raw, entry.count, self.encoding, entry.max_impact)
    }
}

/// One posting list: delta-encoded rows, then the impacts.
///
/// ⚠️ Rows first and impacts second, not interleaved, so a decoder that knows the count can
/// find both halves without a per-posting length. `max` scales `U8` and is ignored by every
/// other encoding.
pub fn write_list(out: &mut Vec<u8>, list: &[(u32, f32)], encoding: ImpactEncoding, max: f32) {
    let mut prev = 0u32;
    for (row, _) in list {
        put_varint(out, u64::from(row.wrapping_sub(prev)));
        prev = *row;
    }
    for (_, w) in list {
        put_impact(out, *w, max, encoding);
    }
}

/// The inverse of [`write_list`], from exactly the bytes the list occupies.
#[must_use]
pub fn read_list(raw: &[u8], count: u32, encoding: ImpactEncoding, max: f32) -> Vec<(u32, f32)> {
    let n = count as usize;
    let mut rows = Vec::with_capacity(n);
    let mut at = 0usize;
    let mut prev = 0u32;
    for _ in 0..n {
        let Some((delta, used)) = get_varint(raw, at) else {
            return Vec::new();
        };
        at += used;
        prev = prev.wrapping_add(delta as u32);
        rows.push(prev);
    }
    let mut out = Vec::with_capacity(n);
    if encoding == ImpactEncoding::Varint {
        for row in rows {
            let Some((v, used)) = get_varint(raw, at) else {
                return out;
            };
            at += used;
            #[expect(
                clippy::cast_precision_loss,
                reason = "exact to 2^24, which is what the encoding promises"
            )]
            out.push((row, v as f32));
        }
        return out;
    }
    let w = encoding.width();
    for (i, row) in rows.into_iter().enumerate() {
        let Some(b) = raw.get(at + i * w..at + (i + 1) * w) else {
            return out;
        };
        out.push((row, read_impact(b, max, encoding)));
    }
    out
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(raw: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut v = 0u64;
    let mut shift = 0u32;
    let mut used = 0usize;
    loop {
        let b = *raw.get(at + used)?;
        used += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some((v, used));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the quantization is the point; the bound is asserted"
)]
fn put_impact(out: &mut Vec<u8>, w: f32, max: f32, encoding: ImpactEncoding) {
    match encoding {
        ImpactEncoding::U8 => {
            // ⚠️ Signed, centred on 128. Clamping negatives to zero would be a silent data
            // loss for any model that emits them, and costs nothing to avoid.
            let scaled = if max > 0.0 { (w / max) * 127.0 } else { 0.0 };
            out.push(((scaled.round() as i32).clamp(-127, 127) + 128) as u8);
        }
        ImpactEncoding::F16 => out.extend_from_slice(&f32_to_f16(w).to_le_bytes()),
        ImpactEncoding::F32 => out.extend_from_slice(&w.to_le_bytes()),
        #[expect(
            clippy::cast_sign_loss,
            reason = "a varint impact is a count; a negative one is not representable and is \
                      clamped rather than wrapped"
        )]
        ImpactEncoding::Varint => put_varint(out, w.max(0.0).round() as u64),
    }
}

fn read_impact(b: &[u8], max: f32, encoding: ImpactEncoding) -> f32 {
    match encoding {
        ImpactEncoding::U8 => b.first().map_or(0.0, |c| {
            (f32::from(i32::from(*c) as i16 - 128) / 127.0) * max
        }),
        ImpactEncoding::F16 => b
            .get(..2)
            .and_then(|x| x.try_into().ok())
            .map_or(0.0, |x| f16_to_f32(u16::from_le_bytes(x))),
        ImpactEncoding::F32 => b
            .get(..4)
            .and_then(|x| x.try_into().ok())
            .map_or(0.0, f32::from_le_bytes),
        // Unreachable: `read_list` decodes varints itself, because they have no width to
        // slice by. Answering 0.0 rather than panicking keeps the function total.
        ImpactEncoding::Varint => 0.0,
    }
}

/// IEEE binary16, round-to-nearest, hand-rolled.
///
/// ⚠️ A crate would do this; `half` is a dependency for forty lines of bit arithmetic that
/// `f16_round_trips_within_its_precision` pins directly. Subnormals and overflow are the two
/// cases a naive version gets wrong, and both are asserted.
#[expect(
    clippy::cast_possible_truncation,
    reason = "every truncation here is a deliberate field width"
)]
fn f32_to_f16(x: f32) -> u16 {
    let bits = x.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x007f_ffff;
    if exp == 0xff {
        // Inf or NaN: keep a non-zero mantissa so a NaN stays a NaN.
        return sign | 0x7c00 | if mant == 0 { 0 } else { 0x0200 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        // Subnormal: shift the implicit leading 1 back in.
        let m = mant | 0x0080_0000;
        let shift = (14 - e) as u32;
        let half = 1u32 << (shift - 1);
        let v = (m + half + ((m >> shift) & 1)) >> shift;
        return sign | v as u16;
    }
    let half = 0x0000_1000u32;
    let rounded = mant + half + ((mant >> 13) & 1);
    // Rounding can carry into the exponent, which is why it is added before the shift.
    let e = e + ((rounded >> 23) & 1) as i32;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    sign | ((e as u16) << 10) | ((rounded >> 13) & 0x3ff) as u16
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h & 0x8000) << 16;
    let exp = u32::from((h >> 10) & 0x1f);
    let mant = u32::from(h & 0x3ff);
    let bits = match exp {
        0 if mant == 0 => sign,
        0 => {
            // Subnormal: no implicit leading 1, so normalise by hand. The value is
            // `mant * 2^-24`; its top set bit sits at `31 - lz`, which fixes both the
            // exponent (`134 - lz`, biased) and how far left the remaining bits shift.
            let lz = mant.leading_zeros();
            sign | ((134 - lz) << 23) | ((mant << (lz - 8)) & 0x007f_ffff)
        }
        0x1f => sign | 0x7f80_0000 | (mant << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (mant << 13),
    };
    f32::from_bits(bits)
}
