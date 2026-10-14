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
}

/// Parquet's codec number for snappy.
pub const CODEC_SNAPPY: u8 = 1;
/// Parquet's codec number for gzip.
pub const CODEC_GZIP: u8 = 2;
/// Parquet's codec number for zstd.
pub const CODEC_ZSTD: u8 = 6;

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

    /// Where row `row`'s vector is, or `None` past the column or in a compressed page.
    #[must_use]
    pub fn row_range(&self, row: usize) -> Option<Range<u64>> {
        let row = u64::try_from(row).ok().filter(|r| *r < self.rows)?;
        if !self.extents.is_empty() {
            let i = self
                .extents
                .partition_point(|e| e.first_row <= row)
                .checked_sub(1)?;
            let e = self.extents.get(i)?;
            if e.compressed > 0 {
                return None;
            }
            let start = e
                .offset
                .checked_add((row - e.first_row).checked_mul(self.width())?)?;
            return Some(start..start.checked_add(self.width())?);
        }
        let start = self.base.checked_add(row.checked_mul(self.stride)?)?;
        Some(start..start.checked_add(self.width())?)
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
        Self::from_extents(key.as_str(), found.dims, found.len, tag, found.extents)
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
            if e.first_row != next {
                return Err(ExternalError::Shape("runs are not contiguous from row 0"));
            }
            if end > len {
                return Err(ExternalError::Shape("a run ends past the object"));
            }
            next += e.rows;
        }
        if extents.is_empty() {
            return Err(ExternalError::Shape("no runs"));
        }
        Ok(Self {
            key: key.to_owned(),
            base: 0,
            dims,
            stride: width,
            rows: next,
            len,
            tag,
            extents,
        })
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
        let key = Key::new(self.key.clone());
        if store.head(&key).await? != self.len || store.get_tag(&key).await? != self.tag {
            return Err(ExternalError::Changed);
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
        let mut ranges: Vec<Range<u64>> =
            raw_rows.iter().filter_map(|r| self.row_range(*r)).collect();
        let page_at: Vec<(usize, &Extent)> = pages
            .keys()
            .filter_map(|i| self.extents.get(*i).map(|e| (*i, e)))
            .collect();
        ranges.extend(
            page_at
                .iter()
                .map(|(_, e)| e.offset..e.offset + e.compressed),
        );
        let mut bufs = store.get_ranges(&key, &ranges).await?.into_iter();
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
        for (i, e) in page_at {
            let comp = bufs.next().ok_or(ExternalError::Damaged)?;
            let width = usize::try_from(self.width()).map_err(|_| ExternalError::Damaged)?;
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
