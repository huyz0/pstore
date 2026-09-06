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

/// An attribute value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    /// Signed integer. The type zone maps prune on.
    Int(i64),
    /// UTF-8 string.
    Str(String),
}

/// Whether a document is one this format version can store faithfully.
///
/// ⚠️ Exists so the refusal can happen where documents **enter** rather than where segments
/// are sealed. A write acknowledged and then found unstorable at fold time is a write the
/// caller believes is durable and which will never appear.
pub fn check_storable(d: &Document) -> Result<(), FormatError> {
    if d.vectors.len() > 1 {
        return Err(FormatError::Unsupported(
            "several vector fields are not stored yet (M3b.3)",
        ));
    }
    for (name, field) in &d.vectors {
        match field {
            VectorField::Sparse(_) => {
                return Err(FormatError::Unsupported(
                    "sparse vector fields are not stored yet (M5a)",
                ));
            }
            VectorField::Dense(v) if v.len() > 1 => {
                return Err(FormatError::Unsupported(
                    "a field with several vectors per document is not stored yet (M3b.3)",
                ));
            }
            VectorField::Dense(_) if name != DEFAULT_FIELD => {
                return Err(FormatError::Unsupported(
                    "named vector fields are not stored yet (M3b.3)",
                ));
            }
            VectorField::Dense(_) => {}
        }
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
            // A string equality has no ordering to prune on, so every block could match.
            Self::Eq(_, Value::Str(_)) => true,
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
    /// Per integer column, the block's `(min, max)`. **The pruning input.**
    pub zones: BTreeMap<String, (i64, i64)>,
}
