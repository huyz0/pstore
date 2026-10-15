//! Vectors read in place out of an object the customer owns (M68, OQ-169).
//!
//! A row's vector is addressable by a ranged read exactly when the column is fixed-width and
//! uncompressed, so its range is **computed**, not looked up. The object is **pinned** at
//! the length and CAS tag it had when the column was recorded: Parquet and Lance never mutate
//! a file in place, so a changed object is a different dataset and a read refuses it.

use pstore_blob::{BlobError, BlobStore, Key};
use pstore_types::CasTag;
use std::collections::BTreeMap;
use std::ops::Range;

/// Why a column could not be pinned or read.
#[derive(Debug, thiserror::Error)]
pub enum ExternalError {
    /// The column's shape is impossible or does not fit in the object.
    #[error("external column does not fit: {0}")]
    Shape(&'static str),
    /// The object is no longer the one the column was pinned to.
    #[error("the external object changed since it was pinned")]
    Changed,
    /// The store failed.
    #[error("external object: {0}")]
    Blob(#[from] BlobError),
    /// A compressed page that does not decompress to the rows it should hold (M75).
    #[error("a compressed page is damaged")]
    Damaged,
    /// A Parquet file or column that cannot be read in place.
    #[error("{0}")]
    Parquet(#[from] crate::parquet::ParquetError),
    /// A row the source holds no vector for (M88: a null). It has a number and no vector.
    #[error("row {0} holds no vector")]
    Absent(usize),
    /// A Lance dataset or column that cannot be read in place (M82).
    #[error("{0}")]
    Lance(#[from] crate::lance::LanceError),
}

/// The most extents one column may be located into (M88, M87): a hostile null or deletion
/// pattern otherwise makes the located column as large as the file.
pub(crate) const MAX_EXTENTS: usize = 1 << 22;

/// Whether a column of `n` extents is within [`MAX_EXTENTS`].
pub(crate) fn within_extent_cap(n: usize) -> bool {
    n <= MAX_EXTENTS
}

/// A run of contiguous vectors: rows `first_row..first_row + rows`, the first at `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// The first row the run holds.
    pub first_row: u64,
    /// Rows in the run.
    pub rows: u64,
    /// The byte offset of the run's first vector.
    pub offset: u64,
    /// 0 for a raw run. Otherwise the run is one snappy-compressed page of this many bytes at
    /// `offset` (M75): its rows have no per-row range.
    pub compressed: u64,
    /// How a compressed page is coded (M76): [`CODEC_SNAPPY`], [`CODEC_GZIP`] or [`CODEC_ZSTD`],
    /// Parquet's codec numbers. 0 for a raw run.
    pub codec: u8,
    /// A dictionary-encoded page's decoded body size, once decompressed (M81): its indices, not
    /// vectors. 0 for a page of plain vectors.
    pub plain_len: u64,
    /// The dictionary this page's indices point into (M81), if it is dictionary-encoded. Such a
    /// page has `compressed > 0` whatever its codec: its rows are reached through the page.
    pub dict: Option<DictRef>,
    /// Which object the run lives in (M87): 0 is the column's own, `n` the `n`th of
    /// [`ExternalColumn::more`]. A Parquet column is always 0.
    pub object: u32,
}

/// A further object a column spans (M87): a Lance dataset keeps its rows in several data files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    /// The object's key.
    pub key: String,
    /// Its length when pinned.
    pub len: u64,
    /// Its CAS tag when pinned, if the store has one.
    pub tag: Option<CasTag>,
}

/// Where a chunk's dictionary page is (M81): a plain run of `count` distinct vectors, compressed
/// with the chunk's codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DictRef {
    /// The byte offset of the dictionary page's body.
    pub offset: u64,
    /// Its on-disk length.
    pub compressed: u64,
    /// Distinct vectors in it.
    pub count: u32,
    /// How it is coded: 0, or a `CODEC_*`.
    pub codec: u8,
}

/// Parquet's codec number for snappy.
pub const CODEC_SNAPPY: u8 = 1;
/// Parquet's codec number for gzip.
pub const CODEC_GZIP: u8 = 2;
/// Parquet's codec number for brotli.
pub const CODEC_BROTLI: u8 = 4;
/// Parquet's codec number for zstd.
pub const CODEC_ZSTD: u8 = 6;
/// Parquet's codec number for `LZ4_RAW`: one lz4 block. (The deprecated Hadoop-framed `LZ4`, 5,
/// is not read.)
pub const CODEC_LZ4_RAW: u8 = 7;

/// The `n` indices of a dictionary-encoded page body (M81): one byte of bit width, then Parquet's
/// RLE / bit-packed hybrid. `None` unless the body holds exactly `n` of them, every one below
/// `count`. ⚠️ A run of zero values would not advance, so it is refused rather than looped on.
fn decode_indices(buf: &[u8], n: usize, count: u32) -> Option<Vec<u32>> {
    let width = usize::from(*buf.first()?);
    if width > 32 {
        return None;
    }
    let mut at = 1usize;
    let mut out: Vec<u32> = Vec::with_capacity(n.min(1 << 20));
    while out.len() < n {
        // The run header: a varint whose low bit says bit-packed (1) or repeated (0).
        let (mut header, mut shift) = (0u64, 0u32);
        loop {
            let b = *buf.get(at)?;
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
        let units = usize::try_from(header >> 1).ok().filter(|u| *u > 0)?;
        if header & 1 == 1 {
            // `units` groups of eight values, `width` bits each, least significant bit first.
            let bytes = units.checked_mul(width)?;
            let packed = buf.get(at..at.checked_add(bytes)?)?;
            at += bytes;
            for k in 0..units.checked_mul(8)? {
                if out.len() == n {
                    break;
                }
                let mut v = 0u64;
                for bit in 0..width {
                    let pos = k * width + bit;
                    if packed.get(pos / 8)? >> (pos % 8) & 1 == 1 {
                        v |= 1 << bit;
                    }
                }
                out.push(u32::try_from(v).ok()?);
            }
        } else {
            // One value, repeated `units` times, in `ceil(width / 8)` little-endian bytes.
            let vb = width.div_ceil(8);
            let raw = buf.get(at..at.checked_add(vb)?)?;
            at += vb;
            // The value's bytes, little-endian, in the low end of eight.
            let mut le = [0u8; 8];
            for (to, from) in le.iter_mut().zip(raw) {
                *to = *from;
            }
            let v = u32::try_from(u64::from_le_bytes(le)).ok()?;
            for _ in 0..units.min(n - out.len()) {
                out.push(v);
            }
        }
    }
    out.iter().all(|i| *i < count).then_some(out)
}

/// A compressed page's bytes, exactly `expected` of them or an error (M76). The output is bounded
/// by `expected + 1` while decoding, so a page that lies about its size cannot balloon memory.
fn decompress(codec: u8, comp: &[u8], expected: usize) -> Result<Vec<u8>, ExternalError> {
    use std::io::Read;
    let mut out = Vec::new();
    let cap = u64::try_from(expected).map_err(|_| ExternalError::Damaged)? + 1;
    match codec {
        CODEC_SNAPPY => {
            if snap::raw::decompress_len(comp).map_err(|_| ExternalError::Damaged)? != expected {
                return Err(ExternalError::Damaged);
            }
            out = snap::raw::Decoder::new()
                .decompress_vec(comp)
                .map_err(|_| ExternalError::Damaged)?;
        }
        CODEC_GZIP => {
            flate2::read::GzDecoder::new(comp)
                .take(cap)
                .read_to_end(&mut out)
                .map_err(|_| ExternalError::Damaged)?;
        }
        CODEC_BROTLI => {
            brotli_decompressor::Decompressor::new(comp, 4096)
                .take(cap)
                .read_to_end(&mut out)
                .map_err(|_| ExternalError::Damaged)?;
        }
        // The block decoder allocates `expected` and refuses to write past it.
        CODEC_LZ4_RAW => {
            out =
                lz4_flex::block::decompress(comp, expected).map_err(|_| ExternalError::Damaged)?;
        }
        CODEC_ZSTD => {
            zstd::stream::read::Decoder::new(comp)
                .map_err(|_| ExternalError::Damaged)?
                .take(cap)
                .read_to_end(&mut out)
                .map_err(|_| ExternalError::Damaged)?;
        }
        _ => return Err(ExternalError::Damaged),
    }
    if out.len() == expected {
        Ok(out)
    } else {
        Err(ExternalError::Damaged)
    }
}

/// A column of fixed-width `f32` vectors inside an external object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalColumn {
    /// The object's key.
    pub key: String,
    /// The byte offset of row 0's vector.
    pub base: u64,
    /// Components in a vector.
    pub dims: u32,
    /// Bytes from one row's start to the next's: at least `dims * 4`.
    pub stride: u64,
    /// Rows in the column.
    pub rows: u64,
    /// The object's length when pinned.
    pub len: u64,
    /// The object's CAS tag when pinned, if the store has one.
    pub tag: Option<CasTag>,
    /// The column as runs of contiguous vectors (a Parquet file's data pages, M70). Empty means
    /// the single run `base`, `stride`, `rows`; otherwise `base` and `stride` are unused and the
    /// vectors in a run are `dims * 4` bytes apart.
    pub extents: Vec<Extent>,
    /// Objects beyond the first that extents may name (M87), numbered from 1. Empty for all but
    /// a Lance dataset of several data files.
    pub more: Vec<ObjectRef>,
}

impl ExternalColumn {
    /// Bytes a vector occupies.
    fn width(&self) -> u64 {
        u64::from(self.dims) * 4
    }

    /// The index of the compressed extent holding `row`, if its extent is compressed.
    fn compressed_extent(&self, row: usize) -> Option<usize> {
        let row = u64::try_from(row).ok()?;
        let i = self
            .extents
            .partition_point(|e| e.first_row <= row)
            .checked_sub(1)?;
        self.extents.get(i).filter(|e| e.compressed > 0).map(|_| i)
    }

    /// Whether row `row` holds a vector: false past the column and, for a column read from a
    /// source with nulls (M88), in a gap between runs.
    #[must_use]
    pub fn is_present(&self, row: usize) -> bool {
        let Some(row) = u64::try_from(row).ok().filter(|r| *r < self.rows) else {
            return false;
        };
        if self.extents.is_empty() {
            return true;
        }
        self.extents
            .partition_point(|e| e.first_row <= row)
            .checked_sub(1)
            .and_then(|i| self.extents.get(i))
            .is_some_and(|e| row < e.first_row + e.rows)
    }

    /// Where row `row`'s vector is, or `None` past the column or in a compressed page.
    #[must_use]
    pub fn row_range(&self, row: usize) -> Option<Range<u64>> {
        self.row_location(row).map(|(_, range)| range)
    }

    /// [`Self::row_range`], and which object the range is in (M87).
    fn row_location(&self, row: usize) -> Option<(u32, Range<u64>)> {
        let row = u64::try_from(row).ok().filter(|r| *r < self.rows)?;
        if !self.extents.is_empty() {
            let i = self
                .extents
                .partition_point(|e| e.first_row <= row)
                .checked_sub(1)?;
            let e = self.extents.get(i)?;
            if e.compressed > 0 || row >= e.first_row + e.rows {
                return None;
            }
            let start = e
                .offset
                .checked_add((row - e.first_row).checked_mul(self.width())?)?;
            return Some((e.object, start..start.checked_add(self.width())?));
        }
        let start = self.base.checked_add(row.checked_mul(self.stride)?)?;
        Some((0, start..start.checked_add(self.width())?))
    }

    /// Records the object's length and tag, refusing a column that cannot fit in it.
    ///
    /// # Errors
    /// [`ExternalError::Shape`] for zero dims, a stride under the vector's width, an overflow,
    /// or a last row past the object's end; a store failure.
    pub async fn pin<S: BlobStore>(
        store: &S,
        key: &Key,
        base: u64,
        dims: u32,
        stride: u64,
        rows: u64,
    ) -> Result<Self, ExternalError> {
        let len = store.head(key).await?;
        let tag = store.get_tag(key).await?;
        let col = Self {
            key: key.as_str().to_owned(),
            base,
            dims,
            stride,
            rows,
            len,
            tag,
            extents: Vec::new(),
            more: Vec::new(),
        };
        if dims == 0 {
            return Err(ExternalError::Shape("zero dimensions"));
        }
        if stride < col.width() {
            return Err(ExternalError::Shape("stride is narrower than a vector"));
        }
        if rows > 0 {
            let end = (rows - 1)
                .checked_mul(stride)
                .and_then(|o| o.checked_add(base))
                .and_then(|o| o.checked_add(col.width()))
                .ok_or(ExternalError::Shape("offsets overflow"))?;
            if end > len {
                return Err(ExternalError::Shape("the last row ends past the object"));
            }
        }
        Ok(col)
    }

    /// Pins the column `column` of the Parquet object at `key`, found in its footer (M70).
    ///
    /// # Errors
    /// [`ExternalError::Parquet`] for a file or column that cannot be read in place; a store
    /// failure.
    pub async fn pin_parquet<S: BlobStore>(
        store: &S,
        key: &Key,
        column: &str,
    ) -> Result<Self, ExternalError> {
        let found = crate::parquet::locate(store, key, column).await?;
        let tag = store.get_tag(key).await?;
        if store.head(key).await? != found.len {
            return Err(ExternalError::Changed);
        }
        Self::from_runs(
            key.as_str(),
            found.dims,
            found.len,
            tag,
            found.rows,
            found.extents,
        )
    }

    /// Pins the float32 fixed-size-list column `column` of the Lance dataset whose manifest is at
    /// `manifest` (M82, M87): the column across its data files, with the rows a deletion file
    /// removes left out of every answer.
    ///
    /// # Errors
    /// [`ExternalError::Lance`] for a dataset or column that cannot be read in place; a store
    /// failure.
    pub async fn pin_lance<S: BlobStore>(
        store: &S,
        manifest: &Key,
        column: &str,
    ) -> Result<Self, ExternalError> {
        let found = crate::lance::locate(store, manifest, column).await?;
        let mut objects = Vec::with_capacity(found.objects.len());
        for (key, len) in &found.objects {
            let tag = store.get_tag(key).await?;
            if store.head(key).await? != *len {
                return Err(ExternalError::Changed);
            }
            objects.push(ObjectRef {
                key: key.as_str().to_owned(),
                len: *len,
                tag,
            });
        }
        Self::from_objects(
            objects,
            found.located.dims,
            found.located.rows,
            found.located.extents,
        )
    }

    /// A column of `extents`, checked: non-empty, no zero-row run, rows contiguous from 0,
    /// every run inside the object.
    ///
    /// # Errors
    /// [`ExternalError::Shape`] for any of those.
    pub fn from_extents(
        key: &str,
        dims: u32,
        len: u64,
        tag: Option<CasTag>,
        extents: Vec<Extent>,
    ) -> Result<Self, ExternalError> {
        let mut next = 0u64;
        for e in &extents {
            if e.first_row != next {
                return Err(ExternalError::Shape("runs are not contiguous from row 0"));
            }
            next = next.saturating_add(e.rows);
        }
        Self::from_runs(key, dims, len, tag, next, extents)
    }

    /// [`Self::from_extents`], where the runs may leave **gaps**: rows the source holds no vector
    /// for (M88, nulls). The runs must be in order and not overlap, and the column is `rows` long
    /// (a trailing gap counts).
    ///
    /// # Errors
    /// [`ExternalError::Shape`] for zero dims, no runs, a zero-row run, runs out of order or
    /// overlapping, a run past `rows` or past the object, or an overflow.
    pub fn from_runs(
        key: &str,
        dims: u32,
        len: u64,
        tag: Option<CasTag>,
        rows: u64,
        extents: Vec<Extent>,
    ) -> Result<Self, ExternalError> {
        let one = ObjectRef {
            key: key.to_owned(),
            len,
            tag,
        };
        Self::from_objects(vec![one], dims, rows, extents)
    }

    /// [`Self::from_runs`] over several objects (M87): `objects[0]` is the column's own and the
    /// rest are [`Self::more`]; each extent's `object` indexes the list. Runs are ordered by row
    /// across objects. A run with a dictionary must be in object 0.
    ///
    /// # Errors
    /// As [`Self::from_runs`], and [`ExternalError::Shape`] for no objects or an extent naming an
    /// object that is not in the list.
    pub fn from_objects(
        objects: Vec<ObjectRef>,
        dims: u32,
        rows: u64,
        extents: Vec<Extent>,
    ) -> Result<Self, ExternalError> {
        let mut objects = objects.into_iter();
        let first = objects
            .next()
            .ok_or(ExternalError::Shape("a column has no object"))?;
        let more: Vec<ObjectRef> = objects.collect();
        if dims == 0 {
            return Err(ExternalError::Shape("zero dimensions"));
        }
        let width = u64::from(dims) * 4;
        let mut next = 0u64;
        for e in &extents {
            let end = (if e.compressed > 0 {
                Some(e.compressed)
            } else {
                e.rows.checked_mul(width)
            })
            .and_then(|b| b.checked_add(e.offset))
            .ok_or(ExternalError::Shape("offsets overflow"))?;
            if e.rows == 0 {
                return Err(ExternalError::Shape("a run holds no rows"));
            }
            let len = match usize::try_from(e.object).ok() {
                Some(0) => first.len,
                Some(n) => more.get(n - 1).map(|o| o.len).ok_or(ExternalError::Shape(
                    "a run names an object the column lacks",
                ))?,
                None => {
                    return Err(ExternalError::Shape(
                        "a run names an object the column lacks",
                    ));
                }
            };
            if let Some(d) = e.dict {
                if e.object != 0 {
                    return Err(ExternalError::Shape(
                        "a dictionary page is in the first object",
                    ));
                }
                let dict_end = d
                    .offset
                    .checked_add(d.compressed)
                    .ok_or(ExternalError::Shape("offsets overflow"))?;
                if d.count == 0 || dict_end > len {
                    return Err(ExternalError::Shape(
                        "a dictionary is empty or past the object",
                    ));
                }
                if e.compressed == 0 {
                    return Err(ExternalError::Shape("a dictionary page has no body"));
                }
            }
            if e.first_row < next {
                return Err(ExternalError::Shape("runs overlap or are out of order"));
            }
            if end > len {
                return Err(ExternalError::Shape("a run ends past the object"));
            }
            next = e
                .first_row
                .checked_add(e.rows)
                .ok_or(ExternalError::Shape("offsets overflow"))?;
        }
        if next > rows {
            return Err(ExternalError::Shape("a run ends past the column"));
        }
        if extents.is_empty() {
            return Err(ExternalError::Shape("no runs"));
        }
        Ok(Self {
            key: first.key,
            base: 0,
            dims,
            stride: width,
            rows,
            len: first.len,
            tag: first.tag,
            extents,
            more,
        })
    }

    /// The ranges, each in the object it names, in the order given (M87): one request set an
    /// object, run together.
    async fn fetch<S: BlobStore>(
        &self,
        store: &S,
        ranges: &[(u32, Range<u64>)],
    ) -> Result<Vec<bytes::Bytes>, ExternalError> {
        let mut by_object: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        for (i, (object, _)) in ranges.iter().enumerate() {
            by_object.entry(*object).or_default().push(i);
        }
        let reads = by_object.into_iter().map(|(object, at)| async move {
            let key = match usize::try_from(object).ok() {
                Some(0) => self.key.clone(),
                Some(n) => self
                    .more
                    .get(n - 1)
                    .map(|o| o.key.clone())
                    .ok_or(ExternalError::Damaged)?,
                None => return Err(ExternalError::Damaged),
            };
            let wanted: Vec<Range<u64>> = at
                .iter()
                .filter_map(|i| ranges.get(*i).map(|r| r.1.clone()))
                .collect();
            let got = store.get_ranges(&Key::new(key), &wanted).await?;
            Ok::<_, ExternalError>((at, got))
        });
        let mut out: Vec<Option<bytes::Bytes>> = vec![None; ranges.len()];
        for (at, got) in futures_util::future::try_join_all(reads).await? {
            for (i, b) in at.into_iter().zip(got) {
                if let Some(slot) = out.get_mut(i) {
                    *slot = Some(b);
                }
            }
        }
        out.into_iter()
            .map(|b| b.ok_or(ExternalError::Damaged))
            .collect()
    }

    /// The named rows' vectors, read by ranged read. Duplicates and rows past the column are
    /// dropped; nothing asked reads nothing.
    ///
    /// # Errors
    /// [`ExternalError::Changed`] if the object's length or tag moved since the pin; a store
    /// failure.
    pub async fn read_rows<S: BlobStore>(
        &self,
        store: &S,
        rows: &[usize],
    ) -> Result<BTreeMap<usize, Vec<f32>>, ExternalError> {
        let mut wanted: Vec<usize> = rows
            .iter()
            .copied()
            .filter(|r| u64::try_from(*r).is_ok_and(|r| r < self.rows))
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        if wanted.is_empty() {
            return Ok(BTreeMap::new());
        }
        if let Some(r) = wanted.iter().find(|r| !self.is_present(**r)) {
            return Err(ExternalError::Absent(*r));
        }
        let key = Key::new(self.key.clone());
        if store.head(&key).await? != self.len || store.get_tag(&key).await? != self.tag {
            return Err(ExternalError::Changed);
        }
        for o in &self.more {
            let k = Key::new(o.key.clone());
            if store.head(&k).await? != o.len || store.get_tag(&k).await? != o.tag {
                return Err(ExternalError::Changed);
            }
        }
        // Raw rows by their own range; a compressed row by its whole page, once.
        let mut raw_rows: Vec<usize> = Vec::new();
        let mut pages: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for r in wanted {
            match self.compressed_extent(r) {
                Some(i) => pages.entry(i).or_default().push(r),
                None => raw_rows.push(r),
            }
        }
        // Every range with the object it is in; the answer comes back in this order.
        let mut ranges: Vec<(u32, Range<u64>)> = raw_rows
            .iter()
            .filter_map(|r| self.row_location(*r))
            .collect();
        let page_at: Vec<(usize, &Extent)> = pages
            .keys()
            .filter_map(|i| self.extents.get(*i).map(|e| (*i, e)))
            .collect();
        ranges.extend(
            page_at
                .iter()
                .map(|(_, e)| (e.object, e.offset..e.offset + e.compressed)),
        );
        // The dictionaries those pages point into, each once (M81).
        let dict_at: Vec<DictRef> = page_at
            .iter()
            .filter_map(|(_, e)| e.dict.map(|d| (d.offset, d)))
            .collect::<std::collections::BTreeMap<u64, DictRef>>()
            .into_values()
            .collect();
        ranges.extend(
            dict_at
                .iter()
                .map(|d| (0, d.offset..d.offset + d.compressed)),
        );
        let mut bufs = self.fetch(store, &ranges).await?.into_iter();
        let mut out = BTreeMap::new();
        let floats = |raw: &[u8]| -> Vec<f32> {
            raw.chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap_or([0; 4])))
                .collect()
        };
        for row in &raw_rows {
            let raw = bufs.next().ok_or(ExternalError::Damaged)?;
            out.insert(*row, floats(&raw));
        }
        // `bufs` yields the pages first and the dictionaries after, as `ranges` was built.
        let page_bufs: Vec<_> = (0..page_at.len()).filter_map(|_| bufs.next()).collect();
        let width = usize::try_from(self.width()).map_err(|_| ExternalError::Damaged)?;
        let mut dicts: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        for d in &dict_at {
            let raw = bufs.next().ok_or(ExternalError::Damaged)?;
            let want = usize::try_from(d.count)
                .ok()
                .and_then(|n| n.checked_mul(width))
                .ok_or(ExternalError::Damaged)?;
            let bytes = if d.codec == 0 {
                raw.to_vec()
            } else {
                decompress(d.codec, &raw, want)?
            };
            if bytes.len() != want {
                return Err(ExternalError::Damaged);
            }
            dicts.insert(d.offset, bytes);
        }
        for ((i, e), comp) in page_at.into_iter().zip(page_bufs) {
            if let Some(d) = e.dict {
                let body = if e.codec == 0 {
                    comp.to_vec()
                } else {
                    decompress(e.codec, &comp, usize::try_from(e.plain_len).unwrap_or(0))?
                };
                if u64::try_from(body.len()).ok() != Some(e.plain_len) {
                    return Err(ExternalError::Damaged);
                }
                let n = usize::try_from(e.rows).map_err(|_| ExternalError::Damaged)?;
                let indices = decode_indices(&body, n, d.count).ok_or(ExternalError::Damaged)?;
                let table = dicts.get(&d.offset).ok_or(ExternalError::Damaged)?;
                for r in pages.get(&i).into_iter().flatten() {
                    let k = usize::try_from(*r as u64 - e.first_row)
                        .map_err(|_| ExternalError::Damaged)?;
                    let at = usize::try_from(*indices.get(k).ok_or(ExternalError::Damaged)?)
                        .map_err(|_| ExternalError::Damaged)?
                        * width;
                    let raw = table.get(at..at + width).ok_or(ExternalError::Damaged)?;
                    out.insert(*r, floats(raw));
                }
                continue;
            }
            let expected = usize::try_from(e.rows)
                .ok()
                .and_then(|n| n.checked_mul(width))
                .ok_or(ExternalError::Damaged)?;
            let page = decompress(e.codec, &comp, expected)?;
            for r in pages.get(&i).into_iter().flatten() {
                let at = usize::try_from(*r as u64 - e.first_row)
                    .map_err(|_| ExternalError::Damaged)?
                    * width;
                let raw = page.get(at..at + width).ok_or(ExternalError::Damaged)?;
                out.insert(*r, floats(raw));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::decode_indices;

    /// An RLE run: `n` copies of `v`, `width` bits (so `ceil(width / 8)` bytes).
    fn rle(n: u64, v: u32, width: usize) -> Vec<u8> {
        let mut b = Vec::new();
        let mut h = n << 1;
        loop {
            let byte = (h & 0x7F) as u8;
            h >>= 7;
            if h == 0 {
                b.push(byte);
                break;
            }
            b.push(byte | 0x80);
        }
        b.extend(&v.to_le_bytes()[..width.div_ceil(8)]);
        b
    }

    /// A bit-packed run of `groups` x 8 values, least significant bit first.
    fn packed(values: &[u32], width: usize) -> Vec<u8> {
        assert_eq!(values.len() % 8, 0);
        let mut b = vec![(((values.len() / 8) << 1) | 1) as u8];
        let mut bits = vec![0u8; values.len() * width / 8];
        for (k, v) in values.iter().enumerate() {
            for bit in 0..width {
                if v >> bit & 1 == 1 {
                    let pos = k * width + bit;
                    bits[pos / 8] |= 1 << (pos % 8);
                }
            }
        }
        b.extend(bits);
        b
    }

    #[test]
    fn rle_and_bit_packed_runs_decode_and_mix() {
        // Three bits wide: 5 x 7, then eight packed values, then 3 x 2.
        let vals = [0, 1, 2, 3, 4, 5, 6, 7];
        let mut body = vec![3u8];
        body.extend(rle(5, 7, 3));
        body.extend(packed(&vals, 3));
        body.extend(rle(3, 2, 3));
        let got = decode_indices(&body, 16, 8).unwrap();
        assert_eq!(got, [7, 7, 7, 7, 7, 0, 1, 2, 3, 4, 5, 6, 7, 2, 2, 2]);
        // Fewer wanted than a run holds: the run is cut, not overrun.
        assert_eq!(decode_indices(&body, 3, 8).unwrap(), [7, 7, 7]);
        assert_eq!(decode_indices(&body, 7, 8).unwrap(), [7, 7, 7, 7, 7, 0, 1]);
        // A run cut by `n` after earlier values: 8 packed, then 2 of 5 repeats.
        let mut tail = vec![3u8];
        tail.extend(packed(&vals, 3));
        tail.extend(rle(5, 6, 3));
        assert_eq!(
            decode_indices(&tail, 10, 8).unwrap(),
            [0, 1, 2, 3, 4, 5, 6, 7, 6, 6]
        );
        // Wide values: a 17-bit packed run and a 3-byte RLE value.
        let wide: Vec<u32> = (0..8).map(|i| 100_000 + i).collect();
        let mut b = vec![17u8];
        b.extend(packed(&wide, 17));
        b.extend(rle(2, 99_999, 17));
        assert_eq!(
            decode_indices(&b, 10, 200_000).unwrap()[..9],
            [
                100_000, 100_001, 100_002, 100_003, 100_004, 100_005, 100_006, 100_007, 99_999
            ]
        );
        // A multi-byte run header (200 repeats) and width 0 (everything is index 0).
        let mut long = vec![2u8];
        long.extend(rle(200, 3, 2));
        assert_eq!(decode_indices(&long, 200, 4).unwrap(), vec![3; 200]);
        let mut zero = vec![0u8];
        zero.extend(rle(4, 0, 0));
        assert_eq!(decode_indices(&zero, 4, 1).unwrap(), [0, 0, 0, 0]);
    }

    #[test]
    fn a_body_that_is_not_n_indices_below_count_is_refused() {
        let mut ok = vec![3u8];
        ok.extend(rle(5, 7, 3));
        assert!(decode_indices(&ok, 5, 8).is_some());
        assert!(
            decode_indices(&ok, 5, 7).is_none(),
            "an index equal to the count"
        );
        assert!(
            decode_indices(&ok, 6, 8).is_none(),
            "fewer indices than rows"
        );
        assert!(decode_indices(&[], 1, 8).is_none(), "no width byte");
        assert!(decode_indices(&[33], 1, 8).is_none(), "a width over 32");
        // 32 is the widest; 33 and 40 would otherwise decode a valid-looking run.
        let mut w32 = vec![32u8];
        w32.extend(rle(2, 5, 32));
        assert_eq!(decode_indices(&w32, 2, 8).unwrap(), [5, 5]);
        for w in [33usize, 40] {
            // An RLE run of two, its value in `ceil(w / 8)` bytes.
            let mut wide = vec![w as u8, 4];
            wide.extend(std::iter::repeat_n(5u8, w.div_ceil(8)));
            assert!(decode_indices(&wide, 2, 8).is_none(), "width {w}");
        }
        assert!(decode_indices(&[3], 1, 8).is_none(), "no run");
        assert!(
            decode_indices(&[3, 0], 1, 8).is_none(),
            "a run of nothing does not loop"
        );
        // A zero-length run is refused even when a good run follows it.
        assert!(
            decode_indices(&[3, 0, 0, 2, 5], 1, 8).is_none(),
            "a zero run"
        );
        assert_eq!(
            decode_indices(&[3, 2, 5], 1, 8).unwrap(),
            [5],
            "a nonzero run"
        );
        // A run header may take six bytes (a padded 2) but not seven.
        assert_eq!(
            decode_indices(&[3, 0x82, 0x80, 0x80, 0x80, 0x80, 0x00, 5], 1, 8).unwrap(),
            [5]
        );
        assert!(
            decode_indices(&[3, 0x82, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00, 5], 1, 8).is_none(),
            "a seven-byte header"
        );
        // A run longer than the rows wanted is cut: 2^28 repeats, three wanted.
        let mut huge = vec![3u8];
        huge.extend(rle(1 << 28, 1, 3));
        assert_eq!(decode_indices(&huge, 3, 8).unwrap(), [1, 1, 1]);
        // A zero-length run is refused even when a good run follows it.
        assert!(
            decode_indices(&[3, 0, 0, 2, 5], 1, 8).is_none(),
            "a zero run"
        );
        assert_eq!(
            decode_indices(&[3, 2, 5], 1, 8).unwrap(),
            [5],
            "a nonzero run"
        );
        // A run header may take six bytes (a padded 2) but not seven.
        assert_eq!(
            decode_indices(&[3, 0x82, 0x80, 0x80, 0x80, 0x80, 0x00, 5], 1, 8).unwrap(),
            [5]
        );
        assert!(
            decode_indices(&[3, 0x82, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00, 5], 1, 8).is_none(),
            "a seven-byte header"
        );
        // A run longer than the rows wanted is cut: 2^28 repeats, three wanted.
        let mut huge = vec![3u8];
        huge.extend(rle(1 << 28, 1, 3));
        assert_eq!(decode_indices(&huge, 3, 8).unwrap(), [1, 1, 1]);
        assert!(
            decode_indices(&[3, 1], 1, 8).is_none(),
            "a packed run of nothing"
        );
        assert!(
            decode_indices(&[3, 0x81], 1, 8).is_none(),
            "a header that never ends"
        );
        assert!(
            decode_indices(&[3, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], 1, 8).is_none(),
            "a header too long to be a run"
        );
        let mut cut = vec![3u8];
        cut.extend(packed(&[1; 8], 3));
        cut.pop();
        assert!(
            decode_indices(&cut, 8, 8).is_none(),
            "a packed run cut short"
        );
        let mut short_rle = vec![3u8];
        short_rle.extend(&rle(5, 7, 3)[..1]);
        assert!(
            decode_indices(&short_rle, 5, 8).is_none(),
            "an RLE value cut short"
        );
    }
}

#[cfg(test)]
mod cap_tests {
    use super::{MAX_EXTENTS, within_extent_cap};

    #[test]
    fn the_extent_cap_is_exact() {
        assert!(within_extent_cap(0));
        assert!(within_extent_cap(MAX_EXTENTS));
        assert!(!within_extent_cap(MAX_EXTENTS + 1));
    }
}
