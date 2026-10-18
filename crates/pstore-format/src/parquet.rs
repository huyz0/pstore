//! Finding a vector column in a Parquet file's footer (M70, OQ-169).
//!
//! Only a column a ranged read can reach qualifies: top-level, `REQUIRED`,
//! `FIXED_LEN_BYTE_ARRAY(dims × 4)`, `UNCOMPRESSED`, `PLAIN`, no dictionary, v1 data pages.
//! Every other shape is refused **with the reason**, because a compressed column needs a
//! decompress, a dictionary a lookup, and a nullable or repeated one level decoding -- none is a
//! computable range, and silently reading one as floats would return noise.

mod list;

use crate::external::{AttrKind, DictRef, Extent};
use pstore_blob::{BlobError, BlobStore, Key};

/// Why a Parquet column could not be located.
#[derive(Debug, thiserror::Error)]
pub enum ParquetError {
    /// Not a Parquet file, or its metadata is damaged.
    #[error("not a readable parquet file: {0}")]
    Malformed(&'static str),
    /// A well-formed file whose column a ranged read cannot reach.
    #[error("column cannot be read in place: {0}")]
    Unsupported(String),
    /// The store failed.
    #[error("parquet object: {0}")]
    Blob(#[from] BlobError),
}

/// A located column: where its vectors are, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// The object's length.
    pub len: u64,
    /// Components in a vector.
    pub dims: u32,
    /// One run of contiguous vectors a data page (or, with nulls, a run of present rows of one),
    /// in row order. Rows between runs hold no vector (M88).
    pub extents: Vec<Extent>,
    /// Rows the column has, nulls included.
    pub rows: u64,
}

/// A Thrift compact-protocol reader over a byte slice.
pub(crate) struct Thrift<'a> {
    b: &'a [u8],
    at: usize,
    depth: usize,
}

const MAX_DEPTH: usize = 32;
const MAX_LIST: u64 = 1 << 20;

impl<'a> Thrift<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0, depth: 0 }
    }

    pub(crate) fn position(&self) -> usize {
        self.at
    }

    fn byte(&mut self) -> Option<u8> {
        let v = *self.b.get(self.at)?;
        self.at += 1;
        Some(v)
    }

    pub(crate) fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..70).step_by(7) {
            let b = self.byte()?;
            v |= u64::from(b & 0x7F).checked_shl(shift)?;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    pub(crate) fn zigzag(&mut self) -> Option<i64> {
        let v = self.varint()?;
        #[allow(clippy::cast_possible_wrap, reason = "zigzag decoding of a u64")]
        Some(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }

    pub(crate) fn binary(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.varint()?).ok()?;
        self.take(n)
    }

    /// The next field's `(id, type)`, or `None` at a struct's stop byte.
    pub(crate) fn field(&mut self, last: i16) -> Option<Option<(i16, u8)>> {
        let b = self.byte()?;
        if b == 0 {
            return Some(None);
        }
        let ty = b & 0x0F;
        let delta = i16::from(b >> 4);
        let id = if delta == 0 {
            i16::try_from(self.zigzag()?).ok()?
        } else {
            last.checked_add(delta)?
        };
        Some(Some((id, ty)))
    }

    /// A list's `(element type, length)`.
    pub(crate) fn list(&mut self) -> Option<(u8, usize)> {
        let b = self.byte()?;
        let ty = b & 0x0F;
        let short = u64::from(b >> 4);
        let n = if short == 15 { self.varint()? } else { short };
        (n <= MAX_LIST).then_some((ty, usize::try_from(n).ok()?))
    }

    /// Skips a value of compact type `ty`.
    pub(crate) fn skip(&mut self, ty: u8) -> Option<()> {
        match ty {
            1 | 2 => Some(()),
            3 => self.byte().map(|_| ()),
            4..=6 => self.varint().map(|_| ()),
            7 => self.take(8).map(|_| ()),
            8 => self.binary().map(|_| ()),
            9 | 10 => {
                let (et, n) = self.list()?;
                (0..n).try_for_each(|_| self.skip_elem(et))
            }
            11 => {
                let n = self.varint()?;
                if n == 0 {
                    return Some(());
                }
                let kv = self.byte()?;
                (0..n.min(MAX_LIST)).try_for_each(|_| {
                    self.skip_elem(kv >> 4)?;
                    self.skip_elem(kv & 0x0F)
                })
            }
            12 => self.skip_struct(),
            _ => None,
        }
    }

    /// Skips a collection element: unlike a struct field, a bool takes a byte of its own.
    fn skip_elem(&mut self, ty: u8) -> Option<()> {
        if matches!(ty, 1 | 2) {
            self.byte().map(|_| ())
        } else {
            self.skip(ty)
        }
    }

    pub(crate) fn skip_struct(&mut self) -> Option<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return None;
        }
        let mut last = 0;
        while let Some((id, ty)) = self.field(last)? {
            last = id;
            self.skip(ty)?;
        }
        self.depth -= 1;
        Some(())
    }
}

#[derive(Default)]
struct Leaf {
    ty: Option<i64>,
    type_length: Option<i64>,
    repetition: Option<i64>,
    name: Vec<u8>,
    children: i64,
    /// The legacy converted type, if the writer set one (M95).
    converted: Option<i64>,
    /// The logical type, if set (M99).
    logical: Option<Logical>,
}

/// A `TIMESTAMP`'s unit (M99).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeUnit {
    Millis,
    Micros,
    Nanos,
}

/// The members of a `LogicalType` union an attribute can be read as (M99).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Logical {
    Integer { signed: bool },
    Date,
    Timestamp(TimeUnit),
    Other,
}

/// A `LogicalType` union (M99): `INTEGER` (field 10), `DATE` (6) and `TIMESTAMP` (8) by what
/// they hold; every other member is [`Logical::Other`].
fn logical_type(t: &mut Thrift<'_>) -> Option<Logical> {
    let mut found = Logical::Other;
    let mut last = 0;
    while let Some((id, ty)) = t.field(last)? {
        last = id;
        found = match (id, ty) {
            (10, 12) => integer(t)?,
            (6, 12) => {
                t.skip_struct()?;
                Logical::Date
            }
            (8, 12) => timestamp(t)?,
            _ => {
                t.skip(ty)?;
                Logical::Other
            }
        };
    }
    Some(found)
}

/// An `IntType`: `bitWidth` (1, unread: the physical type is what is read) and `isSigned` (2).
/// Without `isSigned`, which is required, the sign is unknown and it is no integer.
fn integer(t: &mut Thrift<'_>) -> Option<Logical> {
    let (mut last, mut signed) = (0, None);
    while let Some((id, ty)) = t.field(last)? {
        last = id;
        match (id, ty) {
            (2, 1) => signed = Some(true),
            (2, 2) => signed = Some(false),
            _ => t.skip(ty)?,
        }
    }
    Some(signed.map_or(Logical::Other, |signed| Logical::Integer { signed }))
}

/// A `TimestampType`: `isAdjustedToUTC` (1, ignored) and `unit` (2), a `TimeUnit` union of
/// `MILLIS` (1), `MICROS` (2) and `NANOS` (3). Without a unit it is no instant.
fn timestamp(t: &mut Thrift<'_>) -> Option<Logical> {
    let (mut last, mut unit) = (0, None);
    while let Some((id, ty)) = t.field(last)? {
        last = id;
        if (id, ty) == (2, 12) {
            let mut inner = 0;
            unit = None;
            while let Some((uid, uty)) = t.field(inner)? {
                inner = uid;
                unit = match (uid, uty) {
                    (1, 12) => Some(TimeUnit::Millis),
                    (2, 12) => Some(TimeUnit::Micros),
                    (3, 12) => Some(TimeUnit::Nanos),
                    _ => None,
                };
                t.skip(uty)?;
            }
        } else {
            t.skip(ty)?;
        }
    }
    Some(unit.map_or(Logical::Other, Logical::Timestamp))
}

/// The logical type a legacy converted type stands for, when no logical type is set.
fn from_converted(c: i64) -> Logical {
    match c {
        CONVERTED_INT_8..=CONVERTED_INT_64 => Logical::Integer { signed: true },
        CONVERTED_UINT_8..=CONVERTED_UINT_64 => Logical::Integer { signed: false },
        CONVERTED_DATE => Logical::Date,
        CONVERTED_TIMESTAMP_MILLIS => Logical::Timestamp(TimeUnit::Millis),
        CONVERTED_TIMESTAMP_MICROS => Logical::Timestamp(TimeUnit::Micros),
        _ => Logical::Other,
    }
}

/// What a scalar attribute column is (M99): its physical type read as its logical type when one
/// is set, else as its converted type. A physical value that is not the value (a decimal, a time
/// of day, an interval ...), or a logical type on a physical type it cannot sit on, is refused.
fn scalar_kind(
    ty: Option<i64>,
    converted: Option<i64>,
    logical: Option<Logical>,
) -> Result<AttrKind, String> {
    use AttrKind as K;
    let Some(physical @ (TYPE_INT32 | TYPE_INT64 | TYPE_FLOAT | TYPE_DOUBLE)) = ty else {
        return Err(format!(
            "the column is {}, not INT32, INT64, FLOAT or DOUBLE",
            type_name(ty)
        ));
    };
    let logical = logical.or_else(|| converted.map(from_converted));
    let int = |signed| Some(Logical::Integer { signed });
    match (physical, logical) {
        (TYPE_INT32, None) => Ok(K::Int32),
        (TYPE_INT32, l) if l == int(true) => Ok(K::Int32),
        (TYPE_INT32, l) if l == int(false) => Ok(K::UInt32),
        (TYPE_INT32, Some(Logical::Date)) => Ok(K::Date),
        (TYPE_INT64, None) => Ok(K::Int64),
        (TYPE_INT64, l) if l == int(true) => Ok(K::Int64),
        (TYPE_INT64, l) if l == int(false) => Ok(K::UInt64),
        (TYPE_INT64, Some(Logical::Timestamp(TimeUnit::Millis))) => Ok(K::TimestampMillis),
        (TYPE_INT64, Some(Logical::Timestamp(TimeUnit::Micros))) => Ok(K::TimestampMicros),
        (TYPE_INT64, Some(Logical::Timestamp(TimeUnit::Nanos))) => Ok(K::TimestampNanos),
        (TYPE_FLOAT, None) => Ok(K::Float),
        (TYPE_DOUBLE, None) => Ok(K::Double),
        _ => Err(format!(
            "the column has a logical type its {} value does not carry: only integers, dates and timestamps are read",
            type_name(ty)
        )),
    }
}

fn schema_element(t: &mut Thrift<'_>) -> Option<Leaf> {
    let mut e = Leaf::default();
    let mut last = 0;
    while let Some((id, ty)) = t.field(last)? {
        last = id;
        match (id, ty) {
            (1, 5) => e.ty = Some(t.zigzag()?),
            (2, 5) => e.type_length = Some(t.zigzag()?),
            (3, 5) => e.repetition = Some(t.zigzag()?),
            (4, 8) => e.name = t.binary()?.to_vec(),
            (5, 5) => e.children = t.zigzag()?,
            (6, 5) => e.converted = Some(t.zigzag()?),
            (10, 12) => e.logical = Some(logical_type(t)?),
            _ => t.skip(ty)?,
        }
    }
    Some(e)
}

#[derive(Default)]
struct ChunkMeta {
    ty: Option<i64>,
    encodings: Vec<i64>,
    path: Vec<Vec<u8>>,
    codec: Option<i64>,
    num_values: i64,
    data_page_offset: Option<i64>,
    dictionary_page_offset: Option<i64>,
    total_compressed: i64,
}

fn chunk_meta(t: &mut Thrift<'_>) -> Option<ChunkMeta> {
    let mut m = ChunkMeta::default();
    let mut last = 0;
    while let Some((id, ty)) = t.field(last)? {
        last = id;
        match (id, ty) {
            (1, 5) => m.ty = Some(t.zigzag()?),
            (2, 9) => {
                let (_, n) = t.list()?;
                for _ in 0..n {
                    m.encodings.push(t.zigzag()?);
                }
            }
            (3, 9) => {
                let (_, n) = t.list()?;
                for _ in 0..n {
                    m.path.push(t.binary()?.to_vec());
                }
            }
            (4, 5) => m.codec = Some(t.zigzag()?),
            (5, 6) => m.num_values = t.zigzag()?,
            (7, 6) => m.total_compressed = t.zigzag()?,
            (9, 6) => m.data_page_offset = Some(t.zigzag()?),
            (11, 6) => m.dictionary_page_offset = Some(t.zigzag()?),
            _ => t.skip(ty)?,
        }
    }
    Some(m)
}

/// The flat schema index of the top-level element named `name`, and the number of elements in
/// its subtree (itself included).
fn top_level(schema: &[Leaf], name: &[u8]) -> Option<(usize, usize)> {
    fn subtree(schema: &[Leaf], at: usize) -> Option<usize> {
        let mut size = 1usize;
        for _ in 0..schema.get(at)?.children {
            size = size.checked_add(subtree(schema, at.checked_add(size)?)?)?;
        }
        Some(size)
    }
    let mut at = 1usize;
    while let Some(e) = schema.get(at) {
        let size = subtree(schema, at).filter(|s| *s != 0)?;
        if e.name == name {
            return Some((at, size));
        }
        at = at.checked_add(size)?;
    }
    None
}

/// The path of names from the top-level column `column` down through single children to its
/// leaf (just `[column]` for a flat column): what a chunk's `path_in_schema` is.
fn leaf_path(schema: &[Leaf], column: &[u8]) -> Vec<Vec<u8>> {
    let mut path = vec![column.to_vec()];
    if let Some((mut at, _)) = top_level(schema, column) {
        while schema.get(at).is_some_and(|e| e.children == 1) {
            at = at.saturating_add(1);
            if let Some(e) = schema.get(at) {
                path.push(e.name.clone());
            }
        }
    }
    path
}

/// A column chunk's metadata for each row group, for the one column wanted.
fn row_groups(t: &mut Thrift<'_>, column: &[Vec<u8>]) -> Option<Vec<Option<(ChunkMeta, i64)>>> {
    let (_, n) = t.list()?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut found = None;
        let mut rows = 0;
        let mut last = 0;
        while let Some((id, ty)) = t.field(last)? {
            last = id;
            match (id, ty) {
                (1, 9) => {
                    let (_, cols) = t.list()?;
                    for _ in 0..cols {
                        let mut meta = None;
                        let mut cl = 0;
                        while let Some((cid, cty)) = t.field(cl)? {
                            cl = cid;
                            if cid == 3 && cty == 12 {
                                meta = Some(chunk_meta(t)?);
                            } else {
                                t.skip(cty)?;
                            }
                        }
                        if let Some(m) = meta
                            && m.path == column
                        {
                            found = Some(m);
                        }
                    }
                }
                (3, 6) => rows = t.zigzag()?,
                _ => t.skip(ty)?,
            }
        }
        out.push(found.map(|m| (m, rows)));
    }
    Some(out)
}

struct Footer {
    schema: Vec<Leaf>,
    groups: Vec<Option<(ChunkMeta, i64)>>,
}

fn footer(b: &[u8], column: &[u8]) -> Option<Footer> {
    let mut t = Thrift::new(b);
    let mut schema = Vec::new();
    let mut groups = Vec::new();
    let mut last = 0;
    while let Some((id, ty)) = t.field(last)? {
        last = id;
        match (id, ty) {
            (2, 9) => {
                let (_, n) = t.list()?;
                for _ in 0..n {
                    schema.push(schema_element(&mut t)?);
                }
            }
            (4, 9) => groups = row_groups(&mut t, &leaf_path(&schema, column))?,
            _ => t.skip(ty)?,
        }
    }
    Some(Footer { schema, groups })
}

/// A data page's header.
struct PageHeader {
    /// 0 a v1 data page, 2 a dictionary page, 3 a v2 data page.
    kind: i64,
    values: i64,
    /// On-disk size of the page body.
    compressed: i64,
    /// Size of the body once decompressed.
    uncompressed: i64,
    encoding: i64,
    /// Bytes the header itself takes.
    header_len: usize,
    /// A v2 page's own claims (M80); `None` for a v1 page.
    v2: Option<V2>,
}

/// What a v2 data page header says beyond a v1's.
struct V2 {
    nulls: i64,
    definition_len: i64,
    repetition_len: i64,
    /// Whether the values (not the levels) are compressed with the column's codec.
    is_compressed: bool,
}

const PAGE_V1: i64 = 0;
const PAGE_DICT: i64 = 2;
const PAGE_V2: i64 = 3;

fn page_header(b: &[u8]) -> Option<PageHeader> {
    let mut t = Thrift::new(b);
    let (mut kind, mut un, mut co, mut values, mut enc) = (None, 0, 0, 0, None);
    let mut v2 = None;
    let mut last = 0;
    while let Some((id, ty)) = t.field(last)? {
        last = id;
        match (id, ty) {
            (1, 5) => kind = Some(t.zigzag()?),
            (2, 5) => un = t.zigzag()?,
            (3, 5) => co = t.zigzag()?,
            (5, 12) => {
                let mut l = 0;
                while let Some((did, dty)) = t.field(l)? {
                    l = did;
                    match (did, dty) {
                        (1, 5) => values = t.zigzag()?,
                        (2, 5) => enc = Some(t.zigzag()?),
                        _ => t.skip(dty)?,
                    }
                }
            }
            (7, 12) => {
                // DictionaryPageHeader: values, encoding, is_sorted.
                let mut l = 0;
                while let Some((did, dty)) = t.field(l)? {
                    l = did;
                    match (did, dty) {
                        (1, 5) => values = t.zigzag()?,
                        (2, 5) => enc = Some(t.zigzag()?),
                        _ => t.skip(dty)?,
                    }
                }
            }
            (8, 12) => {
                // DataPageHeaderV2: values, nulls, rows, encoding, def len, rep len, compressed.
                let (mut nulls, mut dl, mut rl, mut comp) = (0, 0, 0, true);
                let mut l = 0;
                while let Some((did, dty)) = t.field(l)? {
                    l = did;
                    match (did, dty) {
                        (1, 5) => values = t.zigzag()?,
                        (2, 5) => nulls = t.zigzag()?,
                        (4, 5) => enc = Some(t.zigzag()?),
                        (5, 5) => dl = t.zigzag()?,
                        (6, 5) => rl = t.zigzag()?,
                        // `true` is the default; only a `false` changes it.
                        (7, 2) => comp = false,
                        _ => t.skip(dty)?,
                    }
                }
                v2 = Some(V2 {
                    nulls,
                    definition_len: dl,
                    repetition_len: rl,
                    is_compressed: comp,
                });
            }
            _ => t.skip(ty)?,
        }
    }
    let kind = kind?;
    let ok = match kind {
        PAGE_V1 | PAGE_DICT => v2.is_none(),
        PAGE_V2 => v2.is_some(),
        _ => false,
    };
    ok.then_some(PageHeader {
        kind,
        values,
        compressed: co,
        uncompressed: un,
        encoding: enc?,
        header_len: t.position(),
        v2,
    })
}

const CONVERTED_DATE: i64 = 6;
const CONVERTED_TIMESTAMP_MILLIS: i64 = 9;
const CONVERTED_TIMESTAMP_MICROS: i64 = 10;
const CONVERTED_UINT_8: i64 = 11;
const CONVERTED_UINT_64: i64 = 14;
const CONVERTED_INT_8: i64 = 15;
const CONVERTED_INT_64: i64 = 18;
const TYPE_INT32: i64 = 1;
const TYPE_INT64: i64 = 2;
const TYPE_FLOAT: i64 = 4;
const TYPE_DOUBLE: i64 = 5;
const TYPE_FLBA: i64 = 7;

/// The name of a physical type, for a refusal.
fn type_name(ty: Option<i64>) -> &'static str {
    match ty {
        Some(0) => "BOOLEAN",
        Some(TYPE_INT32) => "INT32",
        Some(TYPE_INT64) => "INT64",
        Some(TYPE_FLOAT) => "FLOAT",
        Some(TYPE_DOUBLE) => "DOUBLE",
        Some(3) => "INT96",
        Some(6) => "BYTE_ARRAY",
        Some(7) => "FIXED_LEN_BYTE_ARRAY",
        _ => "of an unknown type",
    }
}

const CODEC_NONE: i64 = 0;
const ENC_PLAIN: i64 = 0;
const ENC_PLAIN_DICTIONARY: i64 = 2;
const ENC_RLE: i64 = 3;
const ENC_RLE_DICTIONARY: i64 = 8;
/// The most a dictionary or an index page may claim to decode to (a lie is refused before any
/// allocation).
const MAX_PAGE_BYTES: i64 = 256 << 20;
const REQUIRED: i64 = 0;
const OPTIONAL: i64 = 1;
/// Bytes read for a page header: far more than one holds.
const HEADER_WINDOW: u64 = 256;

fn unsupported<T>(why: impl Into<String>) -> Result<T, ParquetError> {
    Err(ParquetError::Unsupported(why.into()))
}

/// The most runs of present rows one page may split into (M88). A page of alternating null and
/// present rows is the worst case, and each run becomes an extent.
const MAX_RUNS_PER_PAGE: usize = 1 << 20;

/// A run of present rows: `(first row of the page, rows)`.
type Run = (u64, u64);

/// Reads an OPTIONAL page's definition levels (M84, M88).
///
/// Returns the bytes of levels ahead of the values and the runs of rows that hold a value.
/// A v1 page carries the levels inside its body behind a 4-byte length, so only an uncompressed
/// one can be read in place; a v2 page's sit outside the compressed values, whose header says how
/// long they are.
async fn present<S: BlobStore>(
    store: &S,
    key: &Key,
    body: u64,
    on_disk: u64,
    values: u64,
    v2: Option<&V2>,
    codec: u8,
) -> Result<(u64, Vec<Run>), ParquetError> {
    let bad = ParquetError::Malformed;
    let (start, len) = if let Some(v) = v2 {
        (
            body,
            u64::try_from(v.definition_len).map_err(|_| bad("level length"))?,
        )
    } else {
        if codec != 0 {
            return unsupported(
                "an optional v1 page is compressed: its levels are inside the compressed body",
            );
        }
        let n = store.get_range(key, body..body + 4.min(on_disk)).await?;
        let l = n
            .as_ref()
            .try_into()
            .map(u32::from_le_bytes)
            .map_err(|_| bad("level length"))?;
        (body + 4, u64::from(l))
    };
    if len == 0 || start + len > body + on_disk {
        return Err(bad("definition levels run past their page"));
    }
    let raw = store.get_range(key, start..start + len).await?;
    let runs = present_runs(&raw, values).ok_or(bad("definition levels are damaged"))?;
    Ok((if v2.is_some() { len } else { len + 4 }, runs))
}

/// The runs of rows `levels` (Parquet's RLE / bit-packed hybrid at bit width 1, no length
/// prefix) say are present (1), for exactly `n` values with nothing after; `None` if the levels
/// are malformed or split into more than [`MAX_RUNS_PER_PAGE`] runs. Adjacent present runs are
/// one run. Nothing is allocated per value.
fn present_runs(levels: &[u8], n: u64) -> Option<Vec<Run>> {
    let (mut at, mut seen) = (0usize, 0u64);
    let mut runs: Vec<Run> = Vec::new();
    // Appends `count` rows from row `seen`, present or not.
    let mut push = |present: bool, seen: u64, count: u64| -> Option<()> {
        if !present || count == 0 {
            return Some(());
        }
        match runs.last_mut() {
            Some((start, len)) if *start + *len == seen => *len += count,
            _ => {
                if runs.len() >= MAX_RUNS_PER_PAGE {
                    return None;
                }
                runs.push((seen, count));
            }
        }
        Some(())
    };
    while seen < n {
        let (mut header, mut shift) = (0u64, 0u32);
        loop {
            let b = levels.get(at)?;
            at += 1;
            header |= u64::from(b & 0x7F).checked_shl(shift)?;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 35 {
                return None;
            }
        }
        let units = header >> 1;
        if units == 0 {
            return None;
        }
        if header & 1 == 1 {
            // `units` groups of eight bits; the bits past `n` are padding and may be anything.
            let packed = usize::try_from(units)
                .ok()
                .and_then(|b| levels.get(at..at.checked_add(b)?))?;
            at += packed.len();
            let take = (units * 8).min(n - seen);
            for k in 0..take {
                let byte = packed.get(usize::try_from(k / 8).ok()?)?;
                push(byte >> (k % 8) & 1 == 1, seen + k, 1)?;
            }
            seen += take;
        } else {
            // One value, repeated: a single byte at width 1, "present" (1) or "null" (0).
            let value = *levels.get(at)?;
            if value > 1 {
                return None;
            }
            at += 1;
            let take = units.min(n - seen);
            push(value == 1, seen, take)?;
            seen += take;
        }
    }
    (at == levels.len()).then_some(runs)
}

/// Locates `column`'s vectors in the Parquet object at `key`.
///
/// # Errors
/// [`ParquetError::Malformed`] for a file that is not Parquet; [`ParquetError::Unsupported`],
/// naming the reason, for a column a ranged read cannot reach; a store failure.
pub async fn locate<S: BlobStore>(
    store: &S,
    key: &Key,
    column: &str,
) -> Result<Located, ParquetError> {
    locate_flat(store, key, column, false)
        .await
        .map(|(found, _)| found)
}

/// Locates the scalar `INT32`, `INT64`, `FLOAT` or `DOUBLE` column `column` (M95): a value is a
/// "vector" of one or two `f32` lanes, found by the machinery a vector column is, so compression,
/// dictionaries, v2 pages and nulls are read as they are for one.
///
/// # Errors
/// As [`locate`]; a column of another type is refused naming it.
pub async fn locate_scalar<S: BlobStore>(
    store: &S,
    key: &Key,
    column: &str,
) -> Result<(Located, AttrKind), ParquetError> {
    let (found, kind) = locate_flat(store, key, column, true).await?;
    Ok((
        found,
        kind.ok_or(ParquetError::Malformed("a scalar column has no kind"))?,
    ))
}

async fn locate_flat<S: BlobStore>(
    store: &S,
    key: &Key,
    column: &str,
    scalar: bool,
) -> Result<(Located, Option<AttrKind>), ParquetError> {
    let bad = ParquetError::Malformed;
    let len = store.head(key).await?;
    let tail = store.get_suffix(key, 8).await?;
    let (size, magic) = tail.split_at_checked(4).ok_or(bad("short file"))?;
    if magic != b"PAR1" {
        return Err(bad("no PAR1 magic"));
    }
    let size = u64::from(u32::from_le_bytes(
        size.try_into().map_err(|_| bad("footer size"))?,
    ));
    // Magic, the footer, its length and the magic again: nothing shorter is a Parquet file.
    if size == 0 || size + 12 > len {
        return Err(bad("footer size is out of range"));
    }
    let raw = store.get_range(key, len - 8 - size..len - 8).await?;
    let f = footer(&raw, column.as_bytes()).ok_or(bad("footer metadata is damaged"))?;

    // The column: a top-level element of the root with this name.
    let (top, size) = top_level(&f.schema, column.as_bytes())
        .ok_or_else(|| ParquetError::Unsupported(format!("no column named {column:?}")))?;
    let leaf = f.schema.get(top).ok_or(bad("schema"))?;
    if leaf.children > 0 {
        if scalar {
            return unsupported("the column is nested, not a scalar");
        }
        // M91: a list of floats. Anything else nested is refused inside.
        return list::locate_list(store, key, len, &f, column, top, size)
            .await
            .map(|found| (found, None));
    }
    // M95: a scalar is read as a vector of `width / 4` lanes; `ty` is the type every chunk must
    // agree on.
    let (ty, kind) = if scalar {
        let kind = scalar_kind(leaf.ty, leaf.converted, leaf.logical)
            .map_err(ParquetError::Unsupported)?;
        (leaf.ty.ok_or(bad("schema"))?, Some(kind))
    } else {
        if leaf.ty != Some(TYPE_FLBA) {
            return unsupported("the column is not FIXED_LEN_BYTE_ARRAY");
        }
        (TYPE_FLBA, None)
    };

    // M84: an OPTIONAL flat column carries a definition level a value; it is read in place only
    // when every level of every page is "present" (checked page by page below).
    if leaf.repetition != Some(REQUIRED) && leaf.repetition != Some(OPTIONAL) {
        return unsupported("the column is REPEATED: it carries repetition levels");
    }
    let optional = leaf.repetition == Some(OPTIONAL);
    let width = match kind {
        Some(kind) => kind.width() as i64,
        None => leaf
            .type_length
            .filter(|w| *w > 0 && w % 4 == 0)
            .ok_or_else(|| {
                ParquetError::Unsupported("the type length is not a whole number of f32".into())
            })?,
    };
    let dims = u32::try_from(width / 4).map_err(|_| bad("too many dimensions"))?;

    let mut extents = Vec::new();
    let mut first_row = 0u64;
    for group in &f.groups {
        let (m, rows) = group
            .as_ref()
            .ok_or_else(|| ParquetError::Unsupported(format!("a row group lacks {column:?}")))?;
        let codec: u8 = match m.codec {
            Some(CODEC_NONE) => 0,
            Some(c @ (1 | 2 | 4 | 6 | 7)) => u8::try_from(c).map_err(|_| bad("codec"))?,
            other => {
                return unsupported(format!(
                    "the column is compressed with codec {}, and only snappy, gzip, brotli, lz4_raw and zstd are read in place",
                    other.map_or_else(|| "?".to_owned(), |c| c.to_string())
                ));
            }
        };
        if m.encodings
            .iter()
            .any(|e| ![ENC_PLAIN, ENC_RLE, ENC_PLAIN_DICTIONARY, ENC_RLE_DICTIONARY].contains(e))
        {
            return unsupported("the column uses an encoding other than PLAIN or a dictionary");
        }
        if m.ty != Some(ty) {
            return unsupported("a chunk's type disagrees with the schema");
        }
        let data_start = u64::try_from(m.data_page_offset.ok_or(bad("no data page offset"))?)
            .map_err(|_| bad("negative offset"))?;
        // M81: a chunk with a dictionary begins at its dictionary page; some writers record 0.
        let dict_start = m
            .dictionary_page_offset
            .filter(|d| *d > 0)
            .map(|d| u64::try_from(d).map_err(|_| bad("negative offset")))
            .transpose()?;
        let start = dict_start.unwrap_or(data_start);
        let end = start
            .checked_add(u64::try_from(m.total_compressed).map_err(|_| bad("negative size"))?)
            .filter(|e| *e <= len)
            .ok_or(bad("a chunk ends past the file"))?;
        let want = u64::try_from(m.num_values).map_err(|_| bad("negative count"))?;
        if i64::try_from(want).ok() != Some(*rows) {
            return unsupported("a row group's values are not one a row");
        }
        let (mut at, mut got) = (start, 0u64);
        let mut dict: Option<DictRef> = None;
        while got < want {
            if at >= end {
                return Err(bad("pages end before the chunk's values"));
            }
            let window = HEADER_WINDOW.min(end - at);
            let head = store.get_range(key, at..at + window).await?;
            let h = page_header(&head).ok_or(bad("page header"))?;
            let (values, co, un, enc, hlen) = (
                h.values,
                h.compressed,
                h.uncompressed,
                h.encoding,
                h.header_len,
            );
            // M80: a v2 page's levels sit outside the compressed values. A REQUIRED flat column
            // has none, and anything else is not a plain run of vectors.
            let mut page_codec = codec;
            if let Some(v) = &h.v2 {
                if (!optional && v.nulls != 0)
                    || v.repetition_len != 0
                    || (!optional && v.definition_len != 0)
                {
                    return unsupported("a v2 data page carries levels or nulls");
                }
                if !v.is_compressed {
                    page_codec = 0;
                }
            }
            let snappy = page_codec != 0;
            // A chunk that declares a dictionary page begins with one.
            if dict_start == Some(at) && h.kind != PAGE_DICT {
                return Err(bad("a dictionary page out of place"));
            }
            if h.kind == PAGE_DICT {
                // M81: the chunk's dictionary page, first and once; plain vectors.
                if dict_start != Some(at) || dict.is_some() {
                    return Err(bad("a dictionary page out of place"));
                }
                if enc != ENC_PLAIN && enc != ENC_PLAIN_DICTIONARY {
                    return unsupported("a dictionary page is not PLAIN");
                }
                let count = u32::try_from(values)
                    .ok()
                    .filter(|c| *c > 0)
                    .ok_or(bad("dictionary size"))?;
                let plain = i64::from(count) * width;
                let sized = un == plain
                    && plain <= MAX_PAGE_BYTES
                    && if codec == 0 { co == un } else { co > 0 };
                if !sized {
                    return unsupported("a dictionary page is not a plain run of vectors");
                }
                let on_disk = u64::try_from(co).map_err(|_| bad("page size"))?;
                let body = at + hlen as u64;
                if body.checked_add(on_disk).is_none_or(|e| e > end) {
                    return Err(bad("a page runs past its chunk"));
                }
                dict = Some(DictRef {
                    offset: body,
                    compressed: on_disk,
                    count,
                    codec,
                });
                at = body + on_disk;
                continue;
            }
            let values = u64::try_from(values).map_err(|_| bad("page values"))?;
            if got + values > want {
                return Err(bad("pages overshoot the chunk's values"));
            }
            if optional && (enc == ENC_PLAIN_DICTIONARY || enc == ENC_RLE_DICTIONARY) {
                return unsupported(
                    "a dictionary-encoded optional column: its index pages carry levels",
                );
            }
            if enc == ENC_PLAIN_DICTIONARY || enc == ENC_RLE_DICTIONARY {
                // M81: indices into the dictionary, reached by decoding the page.
                let Some(d) = dict else {
                    return Err(bad("a dictionary-encoded page before any dictionary"));
                };
                if un <= 0 || un > MAX_PAGE_BYTES || co <= 0 || (page_codec == 0 && co != un) {
                    return unsupported("an index page has an impossible size");
                }
                let on_disk = u64::try_from(co).map_err(|_| bad("page size"))?;
                let body = at + hlen as u64;
                if values == 0 || body.checked_add(on_disk).is_none_or(|e| e > end) {
                    return Err(bad("a page runs past its chunk"));
                }
                extents.push(Extent {
                    first_row: first_row + got,
                    rows: values,
                    offset: body,
                    compressed: on_disk,
                    codec: page_codec,
                    plain_len: u64::try_from(un).map_err(|_| bad("page size"))?,
                    dict: Some(d),
                    object: 0,
                });
                got += values;
                at = body + on_disk;
                continue;
            }
            if enc != ENC_PLAIN {
                return unsupported("a data page is not PLAIN-encoded");
            }
            let width = u64::from(dims) * 4;
            let on_disk = u64::try_from(co).map_err(|_| bad("page size"))?;
            let body = at + hlen as u64;
            // M84, M88: an OPTIONAL page's definition levels, read once: the bytes they take and
            // the runs of rows that hold a value. A REQUIRED page has none and one run.
            let (lead, runs) = if optional {
                // The levels are read, so the page must be inside its chunk first.
                if values == 0 || body.checked_add(on_disk).is_none_or(|e| e > end) {
                    return Err(bad("a page runs past its chunk"));
                }
                if values.saturating_mul(width) > MAX_PAGE_BYTES as u64 {
                    return unsupported("an optional page claims too many values");
                }
                present(store, key, body, on_disk, values, h.v2.as_ref(), page_codec).await?
            } else {
                (0, vec![(0, values)])
            };
            let held: u64 = runs.iter().map(|r| r.1).sum();
            if let Some(v) = &h.v2
                && optional
                && u64::try_from(v.nulls).ok() != Some(values - held)
            {
                return Err(bad("a v2 page's null count disagrees with its levels"));
            }
            if held < values && snappy {
                return unsupported("a compressed page holds nulls: its values cannot be ranked");
            }
            let plain = held * width;
            // A raw page is exactly its levels and values; a compressed page's values
            // decompress to exactly them. Either way a size that disagrees means another layout.
            let (co_values, un_values) = (co - lead as i64, un - lead as i64);
            let sized = if snappy {
                i64::try_from(plain).ok() == Some(un_values) && co_values > 0
            } else {
                co == un && i64::try_from(plain).ok() == Some(co_values)
            };
            if !sized {
                return unsupported("a data page is compressed or carries levels");
            }
            if values == 0 || body.checked_add(on_disk).is_none_or(|e| e > end) {
                return Err(bad("a page runs past its chunk"));
            }
            // One extent a run of present rows; the rows between are nulls, with no vector.
            let mut rank = 0u64;
            for (run_start, run_len) in &runs {
                extents.push(Extent {
                    first_row: first_row + got + run_start,
                    rows: *run_len,
                    offset: body + lead + rank * width,
                    compressed: if snappy { on_disk - lead } else { 0 },
                    codec: page_codec,
                    plain_len: 0,
                    dict: None,
                    object: 0,
                });
                rank += run_len;
            }
            if !crate::external::within_extent_cap(extents.len()) {
                return unsupported("the column splits into too many runs");
            }
            got += values;
            at = body + on_disk;
        }
        if got != want {
            return Err(bad("pages overshoot the chunk's values"));
        }
        first_row += want;
    }
    Ok((
        Located {
            len,
            dims,
            extents,
            rows: first_row,
        },
        kind,
    ))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    #[test]
    fn levels_give_the_runs_of_present_rows_exactly() {
        // RLE: n x 1, the run header is `n << 1`; one run of everything.
        assert_eq!(present_runs(&[10 << 1, 1], 10), Some(vec![(0, 10)]));
        assert_eq!(
            present_runs(&[0x90, 0x03, 1], 200),
            Some(vec![(0, 200)]),
            "a two-byte header"
        );
        // A run longer than the values wanted is cut; a run of nulls is no run.
        assert_eq!(present_runs(&[50 << 1, 1], 10), Some(vec![(0, 10)]));
        assert_eq!(present_runs(&[10 << 1, 0], 10), Some(vec![]));
        assert_eq!(present_runs(&[0, 1], 10), None, "a run of nothing");
        assert_eq!(
            present_runs(&[10 << 1, 2], 10),
            None,
            "a level that is neither"
        );
        // A run longer than what is left of the page is cut at the page's end, wherever it begins.
        assert_eq!(
            present_runs(&[4 << 1, 1, 50 << 1, 1], 10),
            Some(vec![(0, 10)])
        );
        assert_eq!(
            present_runs(&[4 << 1, 1, 50 << 1, 0], 10),
            Some(vec![(0, 4)])
        );
        assert_eq!(
            present_runs(&[4 << 1, 0, 50 << 1, 1], 10),
            Some(vec![(4, 6)])
        );
        // Nulls split runs; adjacent present runs join.
        assert_eq!(
            present_runs(&[4 << 1, 1, 2 << 1, 0, 4 << 1, 1], 10),
            Some(vec![(0, 4), (6, 4)])
        );
        assert_eq!(
            present_runs(&[4 << 1, 1, 6 << 1, 1], 10),
            Some(vec![(0, 10)]),
            "two present runs in a row are one"
        );
        assert_eq!(
            present_runs(&[4 << 1, 0, 6 << 1, 1], 10),
            Some(vec![(4, 6)]),
            "a leading null run"
        );
        // Bit-packed: one group of eight bits (header 1 << 1 | 1), least significant bit first;
        // bits past `n` are padding and are not read.
        assert_eq!(present_runs(&[3, 0xFF], 8), Some(vec![(0, 8)]));
        assert_eq!(
            present_runs(&[3, 0b0001_1111], 5),
            Some(vec![(0, 5)]),
            "padding"
        );
        assert_eq!(
            present_runs(&[3, 0b0001_0111], 5),
            Some(vec![(0, 3), (4, 1)]),
            "a null bit inside n"
        );
        assert_eq!(present_runs(&[3, 0xFF], 9), None, "ran out of values");
        // A packed run cut by `n` after earlier values: only the bits inside `n` are read.
        assert_eq!(
            present_runs(&[4 << 1, 1, 3, 0b0000_0011], 6),
            Some(vec![(0, 6)])
        );
        assert_eq!(
            present_runs(&[4 << 1, 1, 3, 0b0000_0001], 6),
            Some(vec![(0, 5)])
        );
        // A run header may take six bytes (padded) but not seven.
        assert_eq!(
            present_runs(&[0x94, 0x80, 0x80, 0x80, 0x80, 0x00, 1], 10),
            Some(vec![(0, 10)])
        );
        assert_eq!(
            present_runs(&[0x94, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00, 1], 10),
            None
        );
        // Mixed, then trailing bytes refused, truncation refused.
        assert_eq!(present_runs(&[3, 0xFF, 2 << 1, 1], 10), Some(vec![(0, 10)]));
        assert_eq!(
            present_runs(&[10 << 1, 1, 9], 10),
            None,
            "bytes after the values"
        );
        assert_eq!(present_runs(&[], 1), None);
        assert_eq!(
            present_runs(&[10 << 1], 10),
            None,
            "a run without its value"
        );
        assert_eq!(present_runs(&[0x80], 1), None, "a header that never ends");
        assert_eq!(present_runs(&[0xFF; 8], 1), None, "a header too long");
        assert_eq!(
            present_runs(&[3], 8),
            None,
            "a packed run without its bytes"
        );
    }

    /// `n` alternating present / null rows as one bit-packed run.
    fn alternating(n: u64) -> Vec<u8> {
        let groups = n / 8;
        let mut levels = vec![];
        let mut header = groups << 1 | 1;
        loop {
            let b = (header & 0x7F) as u8;
            header >>= 7;
            if header == 0 {
                levels.push(b);
                break;
            }
            levels.push(b | 0x80);
        }
        levels.extend(std::iter::repeat_n(0b0101_0101u8, groups as usize));
        levels
    }

    #[test]
    fn a_page_of_alternating_rows_is_refused_past_the_run_cap() {
        // One run per present row: the cap itself is allowed, one more is not.
        let at_cap = 2 * MAX_RUNS_PER_PAGE as u64;
        assert_eq!(
            present_runs(&alternating(at_cap), at_cap).map(|r| r.len()),
            Some(MAX_RUNS_PER_PAGE)
        );
        let past = at_cap + 16;
        assert_eq!(present_runs(&alternating(past), past), None);
    }

    #[test]
    fn varints_and_zigzag() {
        let mut t = Thrift::new(&[0x00, 0x7F, 0x80, 0x01, 0xE5, 0x8E, 0x26]);
        assert_eq!(t.varint(), Some(0));
        assert_eq!(t.varint(), Some(127));
        assert_eq!(t.varint(), Some(128));
        assert_eq!(t.varint(), Some(624_485));
        assert_eq!(t.varint(), None, "ran out");
        let mut z = Thrift::new(&[0, 1, 2, 3, 4, 0xFE, 0xFF, 0x03]);
        let got: Vec<i64> = (0..6).map(|_| z.zigzag().unwrap()).collect();
        assert_eq!(got, [0, -1, 1, -2, 2, 32_767]);
        // A varint of eleven continuation bytes is refused, not wrapped.
        assert_eq!(Thrift::new(&[0xFF; 11]).varint(), None);
    }

    #[test]
    fn bools_and_nesting_depth() {
        // A struct field holding true and one holding false take no payload byte.
        let mut t = Thrift::new(&[0x11, 0x12, 0x15, 0x02, 0x00]);
        assert_eq!(t.field(0), Some(Some((1, 1))));
        t.skip(1).unwrap();
        assert_eq!(t.field(1), Some(Some((2, 2))));
        t.skip(2).unwrap();
        assert_eq!(t.field(2), Some(Some((3, 5))));
        assert_eq!(t.zigzag(), Some(1));
        assert_eq!(t.field(3), Some(None));
        // In a list a bool is a byte of its own.
        let mut l = Thrift::new(&[0x32, 1, 0, 1, 0xAA]);
        assert_eq!(l.list(), Some((2, 3)));
        l.skip_elem(2).unwrap();
        l.skip_elem(2).unwrap();
        l.skip_elem(2).unwrap();
        assert_eq!(l.position(), 4);
        // Depth 32 is the deepest allowed; 33 is refused.
        let nest = |n: usize| {
            let mut b: Vec<u8> = std::iter::repeat_n(0x1C, n - 1).collect();
            b.push(0x00);
            b.extend(std::iter::repeat_n(0x00, n - 1));
            b
        };
        assert_eq!(Thrift::new(&nest(32)).skip_struct(), Some(()));
        assert_eq!(Thrift::new(&nest(33)).skip_struct(), None);
        // Depth is released as structs close: a long run of siblings is not nesting.
        let mut sib = vec![0xF9, 0x00]; // list, long form size, element type 12 below
        sib.clear();
        sib.push(0xFC);
        sib.push(100);
        sib.extend(std::iter::repeat_n(0x00, 100));
        let mut t = Thrift::new(&sib);
        assert_eq!(t.skip(9), Some(()));
        assert_eq!(t.position(), sib.len());
    }

    #[test]
    fn a_logical_type_union_decodes_each_member() {
        let decode = |b: &[u8]| logical_type(&mut Thrift::new(b));
        let ts = |u| Some(Logical::Timestamp(u));
        // INTEGER (union field 10): bitWidth 8, isSigned true / false.
        let int = |signed| Some(Logical::Integer { signed });
        assert_eq!(decode(&[0xAC, 0x13, 0x08, 0x11, 0x00, 0x00]), int(true));
        assert_eq!(decode(&[0xAC, 0x13, 0x08, 0x12, 0x00, 0x00]), int(false));
        // Another member (field 5, a struct) that happens to hold a true bool 2 is not an integer.
        assert_eq!(decode(&[0x5C, 0x21, 0x00, 0x00]), Some(Logical::Other));
        // An INTEGER without its required isSigned says nothing of its sign: no integer.
        assert_eq!(
            decode(&[0xAC, 0x13, 0x08, 0x00, 0x00]),
            Some(Logical::Other)
        );
        // DATE (field 6): an empty struct.
        assert_eq!(decode(&[0x6C, 0x00, 0x00]), Some(Logical::Date));
        // TIMESTAMP (field 8): isAdjustedToUTC (1, a bool), then unit (2, a TimeUnit union).
        for (member, unit) in [
            (0x1C, TimeUnit::Millis),
            (0x2C, TimeUnit::Micros),
            (0x3C, TimeUnit::Nanos),
        ] {
            let b = [0x8C, 0x11, 0x1C, member, 0x00, 0x00, 0x00, 0x00];
            assert_eq!(decode(&b), ts(unit), "{unit:?}");
            // The unit first, and the UTC flag (false) after it by its long form.
            let b = [0x8C, 0x2C, member, 0x00, 0x00, 0x02, 0x02, 0x00, 0x00];
            assert_eq!(decode(&b), ts(unit), "{unit:?} reordered");
        }
        // A TIMESTAMP without a unit, or with one no writer defines, is no instant.
        assert_eq!(decode(&[0x8C, 0x11, 0x00, 0x00]), Some(Logical::Other));
        let b = [0x8C, 0x1C, 0x4C, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(decode(&b), Some(Logical::Other));
        // A damaged union is no type at all.
        assert_eq!(decode(&[0x8C, 0x1C]), None);
        assert_eq!(decode(&[0xAC, 0x13]), None);
    }

    #[test]
    fn scalar_kind_is_the_table() {
        use AttrKind as K;
        let signed = Some(Logical::Integer { signed: true });
        let unsigned = Some(Logical::Integer { signed: false });
        let ts = |u| Some(Logical::Timestamp(u));
        let date = Some(Logical::Date);
        // (physical, converted, logical) -> kind.
        let read = [
            (TYPE_INT32, None, None, K::Int32),
            (TYPE_INT32, None, signed, K::Int32),
            (TYPE_INT64, None, None, K::Int64),
            (TYPE_INT64, None, signed, K::Int64),
            (TYPE_FLOAT, None, None, K::Float),
            (TYPE_DOUBLE, None, None, K::Double),
            (TYPE_INT32, None, unsigned, K::UInt32),
            (TYPE_INT64, None, unsigned, K::UInt64),
            (TYPE_INT32, None, date, K::Date),
            (TYPE_INT64, None, ts(TimeUnit::Millis), K::TimestampMillis),
            (TYPE_INT64, None, ts(TimeUnit::Micros), K::TimestampMicros),
            (TYPE_INT64, None, ts(TimeUnit::Nanos), K::TimestampNanos),
            // By the converted type alone: INT_8..INT_64, UINT_8..UINT_64, DATE, TIMESTAMP_*.
            (TYPE_INT32, Some(15), None, K::Int32),
            (TYPE_INT32, Some(16), None, K::Int32),
            (TYPE_INT32, Some(17), None, K::Int32),
            (TYPE_INT64, Some(18), None, K::Int64),
            (TYPE_INT32, Some(11), None, K::UInt32),
            (TYPE_INT32, Some(12), None, K::UInt32),
            (TYPE_INT32, Some(13), None, K::UInt32),
            (TYPE_INT64, Some(14), None, K::UInt64),
            // An unsigned integer on INT64 is a u64, whatever width the writer claimed.
            (TYPE_INT64, Some(13), None, K::UInt64),
            (TYPE_INT32, Some(6), None, K::Date),
            (TYPE_INT64, Some(9), None, K::TimestampMillis),
            (TYPE_INT64, Some(10), None, K::TimestampMicros),
            // The logical type wins over a converted one that contradicts it.
            (TYPE_INT32, Some(11), signed, K::Int32),
            (TYPE_INT64, Some(9), ts(TimeUnit::Nanos), K::TimestampNanos),
            (TYPE_INT64, Some(5), unsigned, K::UInt64),
        ];
        for (ty, converted, logical, kind) in read {
            assert_eq!(
                scalar_kind(Some(ty), converted, logical),
                Ok(kind),
                "{ty} {converted:?} {logical:?}"
            );
        }
        // A logical type on a physical type it cannot sit on, or one that reinterprets the
        // value beyond a number or an instant, is refused naming the physical type.
        let refused = [
            (TYPE_INT64, None, date, "INT64"),
            (TYPE_INT32, None, ts(TimeUnit::Micros), "INT32"),
            (TYPE_FLOAT, None, unsigned, "FLOAT"),
            (TYPE_DOUBLE, None, signed, "DOUBLE"),
            (TYPE_INT32, Some(7), None, "INT32"), // TIME_MILLIS
            (TYPE_INT64, Some(8), None, "INT64"), // TIME_MICROS
            (TYPE_INT32, Some(5), None, "INT32"), // DECIMAL
            (TYPE_INT64, Some(6), None, "INT64"), // DATE on INT64
            (TYPE_INT32, Some(9), None, "INT32"), // TIMESTAMP_MILLIS on INT32
            (TYPE_INT64, None, Some(Logical::Other), "INT64"),
            (TYPE_INT32, Some(15), Some(Logical::Other), "INT32"),
            (TYPE_FLOAT, Some(15), None, "FLOAT"),
        ];
        for (ty, converted, logical, name) in refused {
            let err = scalar_kind(Some(ty), converted, logical).unwrap_err();
            assert!(err.contains("logical type"), "{err}");
            assert!(err.contains(name), "{err}");
        }
        // A physical type no attribute is, by name.
        for (ty, name) in [
            (Some(0), "BOOLEAN"),
            (Some(3), "INT96"),
            (Some(6), "BYTE_ARRAY"),
            (Some(7), "FIXED_LEN_BYTE_ARRAY"),
            (None, "unknown"),
        ] {
            let err = scalar_kind(ty, None, None).unwrap_err();
            assert!(err.contains(name), "{err}");
            assert!(err.contains("not INT32, INT64, FLOAT or DOUBLE"), "{err}");
        }
    }

    #[test]
    fn a_converted_type_is_read_from_the_schema_element() {
        // Field 6 (delta 6, type 5): zigzag(15) = 30, INT_8.
        let b = [0x65, 0x1E, 0x00];
        let leaf = schema_element(&mut Thrift::new(&b)).unwrap();
        assert_eq!(leaf.converted, Some(15));
    }

    #[test]
    fn fields_lists_and_skips() {
        // Field 1 (i32=5), then field 20 by long form (i64), a list of 3 i32, a list of 20 bools,
        // a nested struct with a binary, a map with one pair, a double, a byte, and stop.
        let mut b = vec![0x15, 0x0A]; // delta 1, type 5; zigzag(5)=10
        b.extend([0x06, 0x28, 0x28]); // delta 0, type 6, long-form id zigzag(20), value zigzag(20)
        b.extend([0x19, 0x35, 2, 4, 6]); // delta 1, type 9; list of 3, elem 5
        b.extend([0x19, 0xF2]); // delta 1 list, long-form size, elem type 2
        b.push(20);
        b.extend([2; 20]);
        b.extend([0x1C, 0x18, 0x03, b'a', b'b', b'c', 0x00]); // struct{ binary "abc" }
        b.extend([0x1B, 0x01, 0x55, 2, 4]); // map of 1: key i32, value i32
        b.extend([0x17]);
        b.extend(1.5f64.to_le_bytes());
        b.extend([0x13, 9]); // byte
        b.push(0); // stop
        let mut t = Thrift::new(&b);
        assert_eq!(t.field(0), Some(Some((1, 5))));
        assert_eq!(t.zigzag(), Some(5));
        assert_eq!(t.field(1), Some(Some((20, 6))));
        assert_eq!(t.zigzag(), Some(20));
        assert_eq!(t.field(20), Some(Some((21, 9))));
        assert_eq!(t.list(), Some((5, 3)));
        for want in [1, 2, 3] {
            assert_eq!(t.zigzag(), Some(want));
        }
        // The rest by skipping, to the stop.
        let mut last = 21;
        let mut seen = 0;
        while let Some((id, ty)) = t.field(last).unwrap() {
            last = id;
            t.skip(ty).unwrap();
            seen += 1;
        }
        assert_eq!(seen, 5);
        assert_eq!(t.position(), b.len());
        // Unknown types, truncation and runaway nesting are refused.
        assert_eq!(Thrift::new(&[0]).skip(13), None);
        assert_eq!(Thrift::new(&[0x18, 5, 1]).skip(12), None);
        let deep: Vec<u8> = std::iter::repeat_n(0x1C, 100).collect();
        assert_eq!(Thrift::new(&deep).skip_struct(), None);
        assert_eq!(
            Thrift::new(&[0xF9, 0xFF, 0xFF, 0xFF, 0x7F]).list(),
            None,
            "a list size past the cap"
        );
    }
}
