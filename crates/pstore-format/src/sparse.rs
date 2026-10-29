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
///
/// ⚠️ Names no dimension (M177): a key is a string, and an entry is `Copy` so a lookup's
/// answer can be held while its list is fetched. [`Dictionary::key`] names the dimension of
/// the entry at a position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry {
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
/// M177: keys are strings. Version 1, `u32` dimensions, is still read.
const VERSION: u16 = 2;
const VERSION_U32: u16 = 1;
/// v1: dim(4) offset(8) bytes(4) count(4) max(4).
const ENTRY_U32: usize = 24;
/// v2: key at(8) key len(2) offset(8) bytes(4) count(4) max(4); the keys follow the table.
///
/// ⚠️ The key offset is a `u64` (M178's review): no bound on a segment's vocabulary is
/// enforced, because a refusal at fold would block every later fold of the index, so no
/// vocabulary may overflow the offset either.
const ENTRY: usize = 30;
const HEADER: usize = 8 + 2 + 1 + 4;
/// v2's header also holds the key area's length, a `u64`.
const HEADER_KEYS: usize = HEADER + 8;

/// Builds the postings and the dictionary for one sparse field.
///
/// ⚠️ `docs` is taken in **segment row order**, not document order, and the postings address
/// segment rows. A caller that clusters its dense field reorders the rows; handing this the
/// unreordered slice produces a posting list that is internally consistent and points at the
/// wrong documents — which no round-trip of the list can see.
#[must_use]
pub fn build(docs: &[Document], field: &str, encoding: ImpactEncoding) -> Postings {
    // Key -> (row, weight), ascending in both by construction: rows are visited in order and
    // a BTreeMap orders the keys by their bytes, which is `str`'s order.
    let mut by_key: std::collections::BTreeMap<&str, Vec<(u32, f32)>> =
        std::collections::BTreeMap::new();
    for (row, d) in docs.iter().enumerate() {
        let Some(VectorField::Sparse(pairs)) = d.vectors.get(field) else {
            continue;
        };
        for (key, impact) in pairs {
            by_key
                .entry(key.as_str())
                .or_default()
                .push((row as u32, impact.get()));
        }
    }

    let mut section: Vec<u8> = Vec::new();
    let mut entries: Vec<(&str, Entry)> = Vec::with_capacity(by_key.len());
    for (key, list) in by_key {
        let offset = section.len() as u64;
        let max_impact = list.iter().fold(0.0f32, |m, (_, w)| m.max(w.abs()));
        write_list(&mut section, &list, encoding, max_impact);
        entries.push((
            key,
            Entry {
                offset,
                bytes: (section.len() as u64 - offset) as u32,
                count: list.len() as u32,
                max_impact,
            },
        ));
    }

    let keys_len: usize = entries.iter().map(|(k, _)| k.len()).sum();
    let mut dictionary = Vec::with_capacity(HEADER_KEYS + entries.len() * ENTRY + keys_len);
    dictionary.extend_from_slice(MAGIC);
    dictionary.extend_from_slice(&VERSION.to_le_bytes());
    dictionary.push(encoding.tag());
    dictionary.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    dictionary.extend_from_slice(&(keys_len as u64).to_le_bytes());
    let mut at = 0u64;
    for (k, e) in &entries {
        dictionary.extend_from_slice(&at.to_le_bytes());
        // `check_storable` bounds a key at `SPARSE_KEY_MAX`, which is `u16::MAX`.
        dictionary.extend_from_slice(&(k.len() as u16).to_le_bytes());
        dictionary.extend_from_slice(&e.offset.to_le_bytes());
        dictionary.extend_from_slice(&e.bytes.to_le_bytes());
        dictionary.extend_from_slice(&e.count.to_le_bytes());
        dictionary.extend_from_slice(&e.max_impact.to_le_bytes());
        at += k.len() as u64;
    }
    for (k, _) in &entries {
        dictionary.extend_from_slice(k.as_bytes());
    }
    Postings {
        section,
        dictionary,
    }
}

/// Every row's `(key, impact)` pairs, reconstructed from a whole postings section.
///
/// ⚠️ The inverted index turned back the right way up, which is what a compaction needs and
/// **only** a compaction: it is `O(postings)` in time and bytes, against a query's
/// `O(the query's own lists)`. Used by `Segment::scan`, never by a retriever.
#[must_use]
pub fn transpose(
    dict: &Dictionary,
    section: &[u8],
    rows: usize,
) -> Vec<Vec<(String, crate::Impact)>> {
    let mut out = vec![Vec::new(); rows];
    for (i, e) in dict.entries.iter().enumerate() {
        let Some(raw) = section.get(e.offset as usize..e.offset as usize + e.bytes as usize) else {
            continue;
        };
        let key = dict.key(i);
        for (row, impact) in dict.decode_list(e, raw) {
            if let Some(slot) = out.get_mut(row as usize) {
                slot.push((key.to_owned(), crate::Impact::new(impact)));
            }
        }
    }
    // Ascending by key bytes, as the entries are visited, so each row's pairs come out sorted
    // without a sort here.
    out
}

/// The term dictionary: fixed-width entries, sorted by key bytes.
///
/// ⚠️ Fixed width is what makes a lookup a binary search. A variable-width entry would force
/// a scan of the whole table per query term — 600 KB at 30,000 terms, per term, per query.
#[derive(Debug, Clone)]
pub struct Dictionary {
    encoding: ImpactEncoding,
    /// The key bytes, once, and each key's span in them, as M32's term dictionary holds
    /// its terms (M119): a `String` a key was an allocation a key on every open.
    ///
    /// ⚠️ Decoded once, so the lookup is `slice::binary_search_by` rather than a
    /// hand-written loop. Mutation testing found the reason: `lo < hi` flipped to `lo <= hi`
    /// does not return a wrong answer, it **fails to terminate** — on a query path. Rung 1 of
    /// the gate ladder: the condition that can be wrong no longer exists.
    blob: Vec<u8>,
    keys: Vec<(u64, u16)>,
    entries: Vec<Entry>,
}

impl Dictionary {
    /// Decodes a dictionary, returning `None` for anything malformed.
    #[must_use]
    pub fn decode(raw: &[u8]) -> Option<Self> {
        if raw.get(..8)? != MAGIC {
            return None;
        }
        let encoding = ImpactEncoding::from_tag(*raw.get(10)?)?;
        let count = u32::from_le_bytes(raw.get(11..15)?.try_into().ok()?) as usize;
        let me = match u16::from_le_bytes(raw.get(8..10)?.try_into().ok()?) {
            VERSION => Self::decode_keys(raw, encoding, count)?,
            VERSION_U32 => Self::decode_u32(raw, encoding, count)?,
            _ => return None,
        };
        // A table that is not sorted cannot be searched, and searching it anyway answers
        // "absent" for terms that are present — a silent recall loss rather than a failure.
        // Strictly: a key twice is two lists for one dimension.
        if !(1..me.keys.len()).all(|i| me.key(i - 1).as_bytes() < me.key(i).as_bytes()) {
            return None;
        }
        Some(me)
    }

    /// Version 2: each entry names its key's span in the key area after the table.
    fn decode_keys(raw: &[u8], encoding: ImpactEncoding, count: usize) -> Option<Self> {
        let keys_len =
            usize::try_from(u64::from_le_bytes(raw.get(15..23)?.try_into().ok()?)).ok()?;
        // ⚠️ Length checked here, not per lookup. A truncated table decoded lazily would
        // answer some lookups and silently lose the terms past the cut.
        if raw.len() != HEADER_KEYS + count.checked_mul(ENTRY)? + keys_len {
            return None;
        }
        let blob = raw.get(HEADER_KEYS + count * ENTRY..)?.to_vec();
        std::str::from_utf8(&blob).ok()?;
        let mut keys = Vec::with_capacity(count);
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let b = raw.get(HEADER_KEYS + i * ENTRY..HEADER_KEYS + (i + 1) * ENTRY)?;
            let at = u64::from_le_bytes(b.get(0..8)?.try_into().ok()?);
            let len = u16::from_le_bytes(b.get(8..10)?.try_into().ok()?);
            // Each span inside the key area and on a character boundary, so `key` slices.
            let from = usize::try_from(at).ok()?;
            std::str::from_utf8(blob.get(from..from.checked_add(usize::from(len))?)?).ok()?;
            keys.push((at, len));
            entries.push(entry_at(b.get(10..)?)?);
        }
        Some(Self {
            encoding,
            blob,
            keys,
            entries,
        })
    }

    /// Version 1, before M177: a dimension is a `u32`, and reads as its decimal spelling --
    /// the one key a query for it names. Sorted again by those keys' bytes, since `10` sorts
    /// before `9` as a string and after it as a number.
    fn decode_u32(raw: &[u8], encoding: ImpactEncoding, count: usize) -> Option<Self> {
        if raw.len() != HEADER + count.checked_mul(ENTRY_U32)? {
            return None;
        }
        let mut rows: Vec<(u32, Entry)> = Vec::with_capacity(count);
        for i in 0..count {
            let b = raw.get(HEADER + i * ENTRY_U32..HEADER + (i + 1) * ENTRY_U32)?;
            rows.push((
                u32::from_le_bytes(b.get(0..4)?.try_into().ok()?),
                entry_at(b.get(4..)?)?,
            ));
        }
        if !rows.is_sorted_by(|a, b| a.0 < b.0) {
            return None;
        }
        let mut spelled: Vec<(String, Entry)> =
            rows.into_iter().map(|(d, e)| (d.to_string(), e)).collect();
        spelled.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut me = Self {
            encoding,
            blob: Vec::new(),
            keys: Vec::with_capacity(count),
            entries: Vec::with_capacity(count),
        };
        for (k, e) in spelled {
            me.keys.push((me.blob.len() as u64, k.len() as u16));
            me.blob.extend_from_slice(k.as_bytes());
            me.entries.push(e);
        }
        Some(me)
    }

    /// [`Self::decode`] over the fetched buffer (M119).
    #[must_use]
    pub fn decode_bytes(raw: bytes::Bytes) -> Option<Self> {
        Self::decode(&raw)
    }

    /// [`Self::decode_bytes`], kept for `raw` itself (M122), as
    /// [`crate::text::TermDict::decode_shared`] keeps a term dictionary: a sparse leg walked
    /// every dimension of its segment's dictionary on every query.
    #[must_use]
    pub fn decode_shared(raw: &bytes::Bytes) -> Option<std::sync::Arc<Self>> {
        static KEPT: std::sync::LazyLock<crate::shared::ByBuffer<Dictionary>> =
            std::sync::LazyLock::new(|| crate::shared::ByBuffer::new(crate::text::DECODED_BUDGET));
        KEPT.get_or(
            raw,
            // M177: the key bytes are held too, not only the table.
            |d| d.blob.len() + d.keys.len() * (6 + std::mem::size_of::<Entry>()),
            |b| Self::decode_bytes(b.clone()),
        )
    }

    /// How impacts in this segment are stored.
    #[must_use]
    pub fn encoding(&self) -> ImpactEncoding {
        self.encoding
    }

    /// Distinct dimensions in the dictionary.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the field has no postings at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The key of the entry at `i`, or `""` past the end.
    #[must_use]
    pub fn key(&self, i: usize) -> &str {
        self.keys
            .get(i)
            .and_then(|(at, len)| {
                let at = usize::try_from(*at).unwrap_or(usize::MAX);
                self.blob.get(at..at.saturating_add(usize::from(*len)))
            })
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }

    /// Every key, in byte order.
    pub fn keys(&self) -> impl Iterator<Item = &str> + '_ {
        (0..self.keys.len()).map(|i| self.key(i))
    }

    /// One key's entry, or `None` when the vocabulary does not contain it.
    ///
    /// ⚠️ `None` is the **common** case, not an error: a query term the corpus never used
    /// contributes nothing, and must cost nothing — no range, no request, no failure.
    #[must_use]
    pub fn lookup(&self, key: &str) -> Option<Entry> {
        self.keys
            .binary_search_by(|(at, len)| {
                let at = usize::try_from(*at).unwrap_or(usize::MAX);
                self.blob
                    .get(at..at.saturating_add(usize::from(*len)))
                    .unwrap_or_default()
                    .cmp(key.as_bytes())
            })
            .ok()
            .and_then(|i| self.entries.get(i).copied())
    }

    /// Decodes one posting list from exactly the bytes its entry addresses.
    #[must_use]
    pub fn decode_list(&self, entry: &Entry, raw: &[u8]) -> Vec<(u32, f32)> {
        read_list(raw, entry.count, self.encoding, entry.max_impact)
    }
}

/// An entry's fixed fields: offset(8) bytes(4) count(4) max(4).
fn entry_at(b: &[u8]) -> Option<Entry> {
    Some(Entry {
        offset: u64::from_le_bytes(b.get(0..8)?.try_into().ok()?),
        bytes: u32::from_le_bytes(b.get(8..12)?.try_into().ok()?),
        count: u32::from_le_bytes(b.get(12..16)?.try_into().ok()?),
        max_impact: f32::from_le_bytes(b.get(16..20)?.try_into().ok()?),
    })
}

/// One posting list: delta-encoded rows, then the impacts.
///
/// ⚠️ Rows first and impacts second, not interleaved, so a decoder that knows the count can
/// find both halves without a per-posting length. `max` scales `U8` and is ignored by every
/// other encoding. For `U8` it must be the list's largest `|w|`, as `build` computes it: a
/// smaller one, zero included, saturates rather than being treated as "no scale".
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
/// ⚠️ **No guard on `max`, and no wrapping cast** — each had an equivalent mutant, now
/// written out of existence (M8m). A term whose largest magnitude is zero has only zero
/// impacts, and `0.0 / 0.0` is NaN, which Rust's saturating cast makes `0`: the byte the
/// `max > 0.0` guard produced, so `>=` changed nothing. And `v + 128` with `v` in
/// `-127..=127` always fits a `u8`, so it is converted rather than cast: `as u8` wrapped a
/// `- 128` into the very same byte.
fn put_impact(out: &mut Vec<u8>, w: f32, max: f32, encoding: ImpactEncoding) {
    match encoding {
        ImpactEncoding::U8 => {
            // ⚠️ Signed, centred on 128. Clamping negatives to zero would be a silent data
            // loss for any model that emits them, and costs nothing to avoid.
            let scaled = (w / max) * 127.0;
            let v = (scaled.round() as i32).clamp(-127, 127);
            out.push(u8::try_from(v + 128).unwrap_or(128));
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

/// IEEE binary16, **round-to-nearest-even**, hand-rolled.
///
/// ⚠️ **Every `|` here has an equivalent `^` mutant**, and that is a property of the format
/// rather than a gap in the tests: sign, exponent and significand occupy disjoint bits, so OR
/// and XOR agree on every input that can reach them. Excluded by line and column in
/// `.cargo/mutants.toml` (M8m). `f16_encodes_the_nearest_representable_value`
/// covers the rest — it enumerates all 65,536 codes from the IEEE definition and checks
/// 200,000 inputs against them, **exact midpoints included**, which is what killed the shift,
/// mask and carry mutants that a handful of chosen values could not reach. The midpoints are
/// the ones that took longest to get right: a tie is the only input that can tell
/// round-half-to-even from round-half-up, and the random sweep never lands on one.
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
        // ⚠️ `half - 1 + lsb`, not `half + lsb`. Round-half-to-**even** needs the tie to be
        // decided by the retained bit; adding `half` alone rounds every tie up and makes the
        // `& 1` term vestigial. Mutation testing is what found it: three mutants of that term
        // survived, because a tie was the only input that could tell them apart and the
        // differential test accepted either neighbour there.
        let v = (m + half - 1 + ((m >> shift) & 1)) >> shift;
        return sign | v as u16;
    }
    let half = 0x0000_1000u32;
    // ⚠️ Round-half-to-even, and the `- 1` is what makes it so. See the subnormal path above.
    let rounded = mant + half - 1 + ((mant >> 13) & 1);
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
