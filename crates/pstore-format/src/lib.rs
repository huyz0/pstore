//! The immutable segment.
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │ DATA BLOCKS   independently addressable by byte range        │
//! ├──────────────────────────────────────────────────────────────┤
//! │ INDEX SECTION block directory, zone maps, doc-id map         │
//! ├──────────────────────────────────────────────────────────────┤
//! │ FOOTER        fixed size, at a known offset from the END     │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! One `Range: -N` suffix GET fetches the footer and, for a small segment, the whole index
//! section with it — so a cold open is **two** round trips at worst and **one** in the
//! common case. Nothing about a segment is discovered by listing, and nothing needs a side
//! table: the key is enough.

mod codec;
mod docs;
mod reader;
pub mod sparse;
pub mod text;
mod writer;

pub use docs::{decode_docs, decode_rows, encode_docs, encode_rows};
pub use reader::Segment;
pub use writer::{INDEX_BUDGET, SegmentWriter};

/// A footer-addressed region of a segment.
///
/// ⚠️ **Readers skip what they do not find.** A segment written before a section existed
/// simply has no entry for it, and stays valid forever — which matters because segments are
/// immutable, so "migrate the old ones" is never available. Ids are therefore reserved
/// here, not assigned on first use, and **never reused**: a recycled id makes an old
/// segment decode as something it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u16)]
pub enum Section {
    /// Block offsets, row counts and zone maps. Always present.
    Blocks = 1,
    /// Full-precision `f32` vectors, row-major and fixed-width.
    ///
    /// ⚠️ Deliberately **not** in the data blocks. At 384 dimensions this is 1,536 bytes a
    /// row against 64 for the 1-bit code; interleaving them makes every approximate query
    /// pay for precision it discards, and spends the compression the round-trip budget is
    /// built on before it is used.
    Vectors = 2,
    /// RaBitQ 1-bit codes, fixed-width per row.
    RaBitQ = 3,
    /// int8 codes for the rerank rung, fixed-width per row.
    Sq8 = 4,
    /// Reserved: sparse postings (M5).
    SparsePostings = 5,
    /// Reserved: term dictionary (M6).
    TermDict = 6,
    /// Reserved: token positions (M6).
    Positions = 7,
    /// Describes every vector field: name, kind, metric, dims, and row width.
    ///
    /// ⚠️ **Additive.** The names could have gone into the directory entries, and that would
    /// have needed a version bump — breaking every existing segment, against
    /// `modalities-and-sequencing.md` §3's promise that old segments stay valid forever. It
    /// would also have broken `writer.rs`'s fixed-width patch of the `Blocks` entry offset,
    /// silently. As its own section, a reader that has never heard of it is unaffected.
    Fields = 8,
    /// Vectors of the **second and later** fields.
    ///
    /// ⚠️ Field 0 keeps [`Section::Vectors`]. Letting every field share that id looks
    /// additive and is not: `Segment::open` keys sections by id, last wins, so a reader
    /// predating this table would return *another field's* vectors as the segment's — a
    /// wrong answer rather than a skip, in the one direction the mechanism exists to
    /// protect.
    FieldVectors = 9,
    /// RaBitQ codes of the second and later fields.
    FieldRaBitQ = 10,
    /// int8 codes of the second and later fields.
    FieldSq8 = 11,
    /// Full-text postings: per term, rows as delta varints and an exact term frequency.
    ///
    /// ⚠️ A **separate** section from [`Self::SparsePostings`], and OQ-127 answered: a sparse
    /// dimension is a `u32` a model emits, a BM25 term is a string an analyzer produced, and
    /// sharing one space lets them collide. A collision does not fail — it *adds* two
    /// unrelated signals into one posting list.
    TextPostings = 12,
    /// One `u32` token count per row, for BM25's length normalisation.
    Fieldnorms = 13,
    /// Which attribute(s) the text index was built over: a `u32` count, then that many names.
    ///
    /// ⚠️ **Not a [`Self::Fields`] row**, for the reason `text.rs` already records: that table
    /// describes vector fields, and a text row in it is handed to `decode_field`, which reads
    /// postings as `f32` and returns a dense field of noise.
    ///
    /// ⚠️ **Absence means `"text"`**, never "no text field". Every segment written before this
    /// section existed was built over [`text::DEFAULT_TEXT_FIELD`], and reading absence as
    /// empty would turn off the text index of all of them.
    TextFields = 14,
    /// For each **index** row, the **data** row it names: `u32`, little-endian.
    ///
    /// ⚠️ **Two row spaces, and only one of them is the document.** A boundary vector belongs
    /// to two posting lists, so its codes appear twice and the document must appear once —
    /// otherwise `Engine::scan`'s "exactly once" breaks and a compaction re-seals the
    /// duplicates. The code sections therefore stride by the **index** row count and the
    /// blocks by [`Segment::row_count`], and this maps one to the other.
    ///
    /// ⚠️ **Absent means they are the same space**, which is every segment written before this
    /// and every segment built without replication. It is written only when a row is actually
    /// duplicated, so an unreplicated segment's bytes do not move.
    IndexRows = 15,
}

/// How a vector field is laid out in a segment.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldLayout {
    /// The field's name, as the document used it.
    pub name: String,
    /// 0 = dense, 1 = sparse.
    pub kind: u8,
    /// 0 = dot, 1 = cosine, 2 = l2. Named by the schema in
    /// `modalities-and-sequencing.md` §3, which the M3 index hardcoded as dot.
    pub metric: u8,
    /// Components per vector.
    pub dims: u32,
    /// Vectors per row, or **0 for variable** — then the section opens with `rows + 1`
    /// `u64` offsets.
    ///
    /// ⚠️ Fixed width is the common case and stays the fast path: a row-offset table on a
    /// single-vector field costs four bytes a row and buys nothing, and no recall or query
    /// byte gate can see it.
    pub per_row: u32,
    /// Which section id carries this field's vectors.
    pub vectors: u16,
    /// Which section id carries its 1-bit codes.
    pub rabitq: u16,
    /// Which section id carries its int8 codes.
    pub sq8: u16,
}

/// The index section's length, read from a segment's footer.
///
/// Exposed so the "the index fits the suffix read" invariant can be asserted **directly**
/// rather than inferred from a round-trip count. Depth is the consequence; the fit is the
/// cause, and it is what a future change to block sizing breaks first.
#[must_use]
pub fn index_section_len(segment: &[u8]) -> Option<usize> {
    let foot = segment.len().checked_sub(FOOTER_LEN)?;
    let f = segment.get(foot..)?;
    // MAGIC(8) VERSION(2) index_offset(8) index_len(4)
    let lo = f.get(18..22)?;
    Some(u32::from_le_bytes(lo.try_into().ok()?) as usize)
}

use std::collections::BTreeMap;

/// Bytes fetched by the opening suffix read.
///
/// Larger than the footer on purpose: most indexes are small, and their entire index
/// section arrives with the footer, saving the second round trip. Too large and every open
/// over-reads; 8 KiB is roughly the index section of a few-hundred-row segment.
pub const SUFFIX_FETCH: u64 = 8 * 1024;

/// Bytes of fixed-size footer at the very end of a segment.
pub(crate) const FOOTER_LEN: usize = 8 + 2 + 8 + 4 + 4 + 8 + 8;

pub(crate) const MAGIC: &[u8; 8] = b"PSTORESG";
pub(crate) const VERSION: u16 = 1;
/// A segment holding a float or a bool (M9h.1).
///
/// ⚠️ **A version, not only a tag** (spec review). A reader from before M9h.1 ignores bytes
/// after the zones it knows, so given the float table under version 1 it would open the
/// segment, prune on its integer zones alone, and drop a float row it never decoded --
/// silently. Under version 2 it refuses at `open`. A segment holding neither stays version 1,
/// byte for byte.
pub(crate) const VERSION_TYPED: u16 = 2;

/// An attribute value.
///
/// ⚠️ `Eq` and `Ord` are hand-written (M9h.1) because `f64` has neither: a float compares by
/// `f64::total_cmp`, and variants order as declared. That is the **structural** order -- the
/// one a map or a dedupe needs. How a filter or an order-by compares numbers is
/// [`cmp_numbers`], which puts an int and a float on one line.
#[derive(Debug, Clone)]
pub enum Value {
    /// Signed integer. Zone maps prune on it.
    Int(i64),
    /// UTF-8 string.
    Str(String),
    /// A finite float (M9h.1). Zone maps prune on it; NaN and ±∞ are refused by
    /// [`check_storable`].
    Float(f64),
    /// A bool (M9h.1). No zone map: two values prune nothing worth a zone.
    Bool(bool),
}

impl Value {
    fn rank(&self) -> u8 {
        match self {
            Self::Int(_) => 0,
            Self::Str(_) => 1,
            Self::Float(_) => 2,
            Self::Bool(_) => 3,
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a.cmp(b),
            (Self::Str(a), Self::Str(b)) => a.cmp(b),
            (Self::Float(a), Self::Float(b)) => a.total_cmp(b),
            (Self::Bool(a), Self::Bool(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

/// A number: what an int and a float are both compared as.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
}

impl Number {
    /// The number a value holds, if it holds one.
    #[must_use]
    pub fn of(v: &Value) -> Option<Self> {
        match v {
            Value::Int(n) => Some(Self::Int(*n)),
            Value::Float(f) => Some(Self::Float(*f)),
            Value::Str(_) | Value::Bool(_) => None,
        }
    }
}

/// Two numbers compared **exactly**, never cast through `f64` (M9h.1): `2^53 + 1` is greater
/// than `2^53` as a float, which it would equal as one. `-0.0` equals `0.0`. `None` only
/// for a NaN, which `check_storable` keeps out of storage.
#[must_use]
pub fn cmp_numbers(a: Number, b: Number) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Number::Int(x), Number::Int(y)) => Some(x.cmp(&y)),
        (Number::Float(x), Number::Float(y)) => x.partial_cmp(&y),
        (Number::Int(x), Number::Float(y)) => int_vs_float(x, y),
        (Number::Float(x), Number::Int(y)) => int_vs_float(y, x).map(std::cmp::Ordering::reverse),
    }
}

/// `i` against `f`, exactly.
fn int_vs_float(i: i64, f: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    // 2^63 is exact as an f64; every i64 is below it and at least -2^63.
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    if f.is_nan() {
        return None;
    }
    if f >= TWO_63 {
        return Some(Ordering::Less);
    }
    if f < -TWO_63 {
        return Some(Ordering::Greater);
    }
    // In range, so the truncation is exact, and so is the fraction left over.
    let t = f.trunc();
    #[allow(
        clippy::cast_possible_truncation,
        reason = "t is integral and in i64's range"
    )]
    let whole = t as i64;
    Some(i.cmp(&whole).then_with(|| {
        let frac = f - t;
        if frac > 0.0 {
            Ordering::Less
        } else if frac < 0.0 {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }))
}

/// Whether a document is one this format version can store faithfully.
///
/// ⚠️ Exists so the refusal can happen where documents **enter** rather than where segments
/// are sealed. A write acknowledged and then found unstorable at fold time is a write the
/// caller believes is durable and which will never appear.
pub fn check_storable(d: &Document) -> Result<(), FormatError> {
    // ⚠️ Empty, and deliberately still here. It was briefly much wider, as a stopgap: the
    // model gained named, plural and sparse fields before the layout did, and in between a
    // document carrying one was written as *nothing*. M3b.3 made the first two storable and
    // M5a.1 the third, so there is nothing left this format cannot hold.
    //
    // Kept as the door, not deleted, because the refusal belongs where documents ENTER —
    // and there is still one shape the layout cannot hold. A segment addresses its postings
    // through a single `SparsePostings` id; a second sparse field would need its own id pair,
    // the way `FieldVectors` mirrors `Vectors`. Without this, the writer takes the first
    // field in name order and the second is written as **nothing**, which is precisely the
    // silent loss this function exists to make loud.
    // M9h.1: a NaN compares as nothing and ±∞ as a bound no zone map can hold. JSON cannot
    // write either; the engine API can, so the door is here.
    if d.attrs
        .values()
        .any(|v| matches!(v, Value::Float(f) if !f.is_finite()))
    {
        return Err(FormatError::Unsupported(
            "a float attribute must be finite: NaN and infinities are refused",
        ));
    }
    if d.vectors
        .values()
        .filter(|f| matches!(f, VectorField::Sparse(_)))
        .count()
        > 1
    {
        return Err(FormatError::Unsupported(
            "a document may carry at most one sparse field: a second needs its own section \
             id pair (deferred by M5a)",
        ));
    }
    Ok(())
}

/// A sparse posting's weight.
///
/// ⚠️ **Opaque on purpose.** D-72 makes the impact *encoding* the configurable part — u8,
/// f16 or varint depending on the index — so a public `f32` payload would make choosing
/// that encoding a breaking change to `Document` itself. The representation is private and
/// the compiler enforces it, which is rung 1 of the gate ladder: M5a can change what is
/// stored here without any caller noticing.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Impact(f32);

impl Impact {
    /// An impact from a weight.
    #[must_use]
    pub fn new(weight: f32) -> Self {
        Self(weight)
    }

    /// The weight, to whatever precision the current encoding keeps.
    ///
    /// Deliberately not documented as exact: today it round-trips, and a quantized encoding
    /// later will not. A caller that depends on exactness is a caller M5a would break.
    #[must_use]
    pub fn get(self) -> f32 {
        self.0
    }
}

/// One named vector field's contents.
///
/// ⚠️ **Kind-tagged, not a list of dense vectors.** A sparse vector is `(dimension, impact)`
/// pairs; representing one as a dense array over a 30,000-term vocabulary costs **150×** the
/// bytes — 12 TB against 0.08 TB at 100M documents. A model that can only hold dense arrays
/// does not avoid the migration trap, it moves it.
#[derive(Debug, Clone, PartialEq)]
pub enum VectorField {
    /// One vector for a dense field; several for late interaction (D-28).
    Dense(Vec<Vec<f32>>),
    /// `(dimension, impact)` pairs (D-72).
    ///
    /// ⚠️ Representable in v1 and **refused by the writer** — the shape must exist now, the
    /// retriever is M5a. `modalities-and-sequencing.md` §3: "the shape must exist in v1 even
    /// if only `dense` is implemented".
    Sparse(Vec<(u32, Impact)>),
}

impl VectorField {
    /// A single dense vector, the overwhelmingly common case.
    #[must_use]
    pub fn dense(v: Vec<f32>) -> Self {
        Self::Dense(vec![v])
    }

    /// The dense vectors, or empty for a sparse field.
    #[must_use]
    pub fn as_dense(&self) -> &[Vec<f32>] {
        match self {
            Self::Dense(v) => v,
            Self::Sparse(_) => &[],
        }
    }
}

/// The field a single-vector document uses.
///
/// Named rather than implicit: a segment written today must be readable by a reader that
/// knows about many fields, and it can only be if today's one field has a name.
pub const DEFAULT_FIELD: &str = "vector";

/// A row: an id, its named vector fields, and typed attributes.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    /// Unique within its index. Determines the shard, and never changes.
    pub id: String,
    /// ⚠️ **Named and plural.** `modalities-and-sequencing.md` §3 calls a singular `vector`
    /// field "the migration trap", and D-28 says retrofitting *n* vectors into a
    /// one-row-one-vector layout is a rewrite rather than a change.
    pub vectors: BTreeMap<String, VectorField>,
    /// Filterable attributes.
    pub attrs: BTreeMap<String, Value>,
}

impl Document {
    /// A document with one dense vector in [`DEFAULT_FIELD`] and no attributes.
    ///
    /// Kept because it is what almost every caller wants; the general shape is one field
    /// among several, and this is the one-field case spelled conveniently.
    #[must_use]
    pub fn new(id: impl Into<String>, vector: Vec<f32>) -> Self {
        Self {
            id: id.into(),
            vectors: BTreeMap::from([(DEFAULT_FIELD.to_owned(), VectorField::dense(vector))]),
            attrs: BTreeMap::new(),
        }
    }

    /// The document's vectors in one named field, or empty if it has none.
    #[must_use]
    pub fn field(&self, name: &str) -> &[Vec<f32>] {
        self.vectors.get(name).map_or(&[], VectorField::as_dense)
    }

    /// The single dense vector in [`DEFAULT_FIELD`], or empty.
    ///
    /// The bridge for code written against the old singular shape. It is a *convenience*,
    /// not the model: a caller that only ever uses this is a caller that will be surprised
    /// by a document with two fields.
    #[must_use]
    pub fn vector(&self) -> &[f32] {
        self.field(DEFAULT_FIELD)
            .first()
            .map_or(&[][..], Vec::as_slice)
    }
}

/// A predicate over one attribute.
///
/// Deliberately small: M1 needs enough to demonstrate that **zone maps prune blocks**, and
/// a richer expression language would add surface without testing that property harder.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// Attribute equals a value.
    Eq(String, Value),
    /// Integer attribute strictly greater than.
    Gt(String, i64),
    /// Integer attribute strictly less than.
    Lt(String, i64),
}

impl Filter {
    /// The attribute this filter reads.
    #[must_use]
    pub fn column(&self) -> &str {
        match self {
            Self::Eq(c, _) | Self::Gt(c, _) | Self::Lt(c, _) => c,
        }
    }

    /// Whether one row satisfies it.
    #[must_use]
    pub fn matches(&self, doc: &Document) -> bool {
        match self {
            Self::Eq(c, v) => doc.attrs.get(c) == Some(v),
            Self::Gt(c, n) => matches!(doc.attrs.get(c), Some(Value::Int(v)) if v > n),
            Self::Lt(c, n) => matches!(doc.attrs.get(c), Some(Value::Int(v)) if v < n),
        }
    }

    /// Whether a block whose integer column spans `min..=max` **could** contain a match.
    ///
    /// ⚠️ Must never answer `false` for a block that could match: a wrong `false` silently
    /// drops rows, which reads as a recall bug rather than an error. Answering `true` for a
    /// block that cannot match is merely wasted work.
    #[must_use]
    pub fn could_match(&self, min: i64, max: i64) -> bool {
        match self {
            Self::Eq(_, Value::Int(n)) => *n >= min && *n <= max,
            // A string or bool equality has no integer zone to prune on, and this legacy
            // filter matches structurally, so a float never equals an int row here either.
            Self::Eq(_, Value::Str(_) | Value::Float(_) | Value::Bool(_)) => true,
            Self::Gt(_, n) => max > *n,
            Self::Lt(_, n) => min < *n,
        }
    }
}

/// Why a segment could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    /// The bytes ran out mid-field.
    #[error("segment truncated")]
    Truncated,
    /// Something was structurally wrong.
    #[error("segment corrupt: {0}")]
    Corrupt(&'static str),
    /// A document shape the model can express but this format version cannot store.
    ///
    /// ⚠️ Refusal rather than truncation. The model gained named, plural and sparse fields
    /// before the layout did, and in between the writer stored such documents as *nothing* —
    /// silently. A caller cannot recover from a loss it is not told about.
    #[error("cannot store this document: {0}")]
    Unsupported(&'static str),
    /// A field the segment does not carry.
    #[error("no such vector field")]
    UnknownField,
    /// Written by a version this build does not understand.
    #[error("unsupported segment version {0}")]
    UnsupportedVersion(u16),
    /// The index section did not match its recorded checksum.
    #[error("segment checksum mismatch")]
    ChecksumMismatch,
    /// A query's dimension did not match the segment's.
    #[error("dimension mismatch: segment has {expected}, query has {got}")]
    DimensionMismatch {
        /// The segment's dimension.
        expected: usize,
        /// The query's.
        got: usize,
    },
    /// The blob store could not serve it.
    #[error("blob error: {0}")]
    Blob(String),
}

impl From<pstore_blob::BlobError> for FormatError {
    fn from(e: pstore_blob::BlobError) -> Self {
        Self::Blob(e.to_string())
    }
}

/// Where one block lives, and what it holds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BlockMeta {
    pub offset: u64,
    pub len: u32,
    pub rows: u32,
    /// The block's zone maps. **The pruning input.**
    pub zones: Zones,
}

/// One block's zone maps: per attribute, the `(min, max)` of the rows holding it.
///
/// ⚠️ A zone covers only the rows that HAVE the attribute with that type. An absent zone
/// rules nothing out -- the block may be zone-free, or hold that name under another type --
/// **unless** [`Self::complete`]: see there.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Zones {
    /// Integer attributes.
    pub ints: BTreeMap<String, (i64, i64)>,
    /// Float attributes (M9h.1).
    pub floats: BTreeMap<String, (f64, f64)>,
    /// The segment is typed (M9h.1) and says its zone maps are on, so an absent int or float
    /// zone means **no row of that type** holds the name.
    ///
    /// Otherwise the float side rules everything out -- an untyped segment holds no float --
    /// and an absent int zone rules nothing out, as before M9h.1. A typed zone-free segment
    /// has no zone at all, so there the int side rules nothing out either.
    pub complete: bool,
}

impl Zones {
    /// Whether some row holding `name` as a number could satisfy `ok(lo, hi)` over its
    /// type's zone. Numbers of both types are checked, because a filter compares them as one.
    #[must_use]
    pub fn could_hold_number(&self, name: &str, ok: impl Fn(Number, Number) -> bool) -> bool {
        let int = self.ints.get(name).map_or(!self.complete, |&(lo, hi)| {
            ok(Number::Int(lo), Number::Int(hi))
        });
        let float = self
            .floats
            .get(name)
            .is_some_and(|&(lo, hi)| ok(Number::Float(lo), Number::Float(hi)));
        int || float
    }
}
