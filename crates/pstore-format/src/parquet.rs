//! Finding a vector column in a Parquet file's footer (M70, OQ-169).
//!
//! Only a column a ranged read can reach qualifies: top-level, `REQUIRED`,
//! `FIXED_LEN_BYTE_ARRAY(dims × 4)`, `UNCOMPRESSED`, `PLAIN`, no dictionary, v1 data pages.
//! Every other shape is refused **with the reason**, because a compressed column needs a
//! decompress, a dictionary a lookup, and a nullable or repeated one level decoding -- none is a
//! computable range, and silently reading one as floats would return noise.

use crate::external::{DictRef, Extent};
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
    /// One run of contiguous vectors a data page, covering rows `0..` without a gap.
    pub extents: Vec<Extent>,
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

/// A column chunk's metadata for each row group, for the one column wanted.
fn row_groups(t: &mut Thrift<'_>, column: &[u8]) -> Option<Vec<Option<(ChunkMeta, i64)>>> {
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
                            && m.path.len() == 1
                            && m.path.first().is_some_and(|p| p == column)
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
            (4, 9) => groups = row_groups(&mut t, column)?,
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

const TYPE_FLBA: i64 = 7;
const CODEC_NONE: i64 = 0;
const ENC_PLAIN: i64 = 0;
const ENC_PLAIN_DICTIONARY: i64 = 2;
const ENC_RLE: i64 = 3;
const ENC_RLE_DICTIONARY: i64 = 8;
/// The most a dictionary or an index page may claim to decode to (a lie is refused before any
/// allocation).
const MAX_PAGE_BYTES: i64 = 256 << 20;
const REQUIRED: i64 = 0;
/// Bytes read for a page header: far more than one holds.
const HEADER_WINDOW: u64 = 256;

fn unsupported<T>(why: impl Into<String>) -> Result<T, ParquetError> {
    Err(ParquetError::Unsupported(why.into()))
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

    // The leaf: a direct child of the root with this name, and nothing nested.
    let leaf = f
        .schema
        .iter()
        .skip(1)
        .find(|e| e.name == column.as_bytes())
        .ok_or_else(|| ParquetError::Unsupported(format!("no column named {column:?}")))?;
    if leaf.children > 0 {
        return unsupported("the column is a group (nested or a list): it has repetition levels");
    }
    if leaf.ty != Some(TYPE_FLBA) {
        return unsupported("the column is not FIXED_LEN_BYTE_ARRAY");
    }
    if leaf.repetition != Some(REQUIRED) {
        return unsupported(
            "the column is not REQUIRED: nullable or repeated columns carry levels",
        );
    }
    let width = leaf
        .type_length
        .filter(|w| *w > 0 && w % 4 == 0)
        .ok_or_else(|| {
            ParquetError::Unsupported("the type length is not a whole number of f32".into())
        })?;
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
        if m.ty != Some(TYPE_FLBA) {
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
                if v.nulls != 0 || v.definition_len != 0 || v.repetition_len != 0 {
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
                });
                got += values;
                at = body + on_disk;
                continue;
            }
            if enc != ENC_PLAIN {
                return unsupported("a data page is not PLAIN-encoded");
            }
            let plain = values * u64::from(dims) * 4;
            // A raw page is exactly its values; a snappy page decompresses to exactly them.
            // Either way a size that disagrees means levels or another layout.
            let sized = if snappy {
                i64::try_from(plain).ok() == Some(un) && co > 0
            } else {
                co == un && i64::try_from(plain).ok() == Some(co)
            };
            if !sized {
                return unsupported("a data page is compressed or carries levels");
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
                compressed: if snappy { on_disk } else { 0 },
                codec: page_codec,
                plain_len: 0,
                dict: None,
            });
            got += values;
            at = body + on_disk;
        }
        if got != want {
            return Err(bad("pages overshoot the chunk's values"));
        }
        first_row += want;
    }
    Ok(Located { len, dims, extents })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

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
