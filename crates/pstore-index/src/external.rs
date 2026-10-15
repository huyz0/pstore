//! An index over vectors left in the customer's object (M69, OQ-169).
//!
//! The index is **one blob of 1-bit codes** and the pin of the column they were taken from. No
//! float of the customer's data is written. A search scores every code, keeps the `window`
//! best, and reads only those rows out of the customer's object -- through
//! [`ExternalColumn`], which refuses an object that changed -- to return the exact cosine of
//! the top `k`.

use crate::rabitq::{QuantError, Quantizer};
use pstore_blob::{BlobError, BlobStore, Key};
use pstore_format::external::{DictRef, Extent, ExternalColumn, ExternalError, ObjectRef};
use pstore_types::CasTag;

const MAGIC: &[u8; 4] = b"PXI6";

/// Why an external index could not be built, opened or searched.
#[derive(Debug)]
pub enum ExternalIndexError {
    /// The codes blob is not one this version wrote, or is damaged.
    Corrupt(&'static str),
    /// A vector (or the query) has no direction, or the wrong width.
    Vector(&'static str),
    /// The customer's object: changed, missing, or not shaped as pinned.
    Source(ExternalError),
    /// The store failed.
    Blob(BlobError),
}

impl core::fmt::Display for ExternalIndexError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Corrupt(why) => write!(f, "external index corrupt: {why}"),
            Self::Vector(why) => write!(f, "external index vector: {why}"),
            Self::Source(e) => write!(f, "external index source: {e}"),
            Self::Blob(e) => write!(f, "external index store: {e}"),
        }
    }
}

impl core::error::Error for ExternalIndexError {}

impl From<ExternalError> for ExternalIndexError {
    fn from(e: ExternalError) -> Self {
        Self::Source(e)
    }
}

impl From<BlobError> for ExternalIndexError {
    fn from(e: BlobError) -> Self {
        Self::Blob(e)
    }
}

impl From<QuantError> for ExternalIndexError {
    fn from(_: QuantError) -> Self {
        Self::Vector("wrong width")
    }
}

/// Codes over a pinned external column.
#[derive(Debug, Clone)]
pub struct ExternalIndex {
    column: ExternalColumn,
    quantizer: Quantizer,
    /// `rows` codes back to back, each `quantizer.code_len()` bytes.
    codes: Vec<u8>,
}

fn unit(v: &[f32]) -> Option<Vec<f32>> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    (n > 0.0 && n.is_finite()).then(|| v.iter().map(|x| x / n).collect())
}

impl ExternalIndex {
    /// Reads `column` `chunk` rows at a time, encodes each vector, and writes the codes blob at
    /// `codes_key`.
    ///
    /// # Errors
    /// A vector with no direction (zero or non-finite), a changed source, or a store failure.
    pub async fn build<S: BlobStore>(
        store: &S,
        codes_key: &Key,
        column: ExternalColumn,
        chunk: usize,
    ) -> Result<Self, ExternalIndexError> {
        let dims = column.dims as usize;
        let quantizer = Quantizer::new(dims);
        let rows = usize::try_from(column.rows)
            .map_err(|_| ExternalIndexError::Corrupt("too many rows"))?;
        let chunk = chunk.max(1);
        let mut codes = Vec::with_capacity(rows.saturating_mul(quantizer.code_len()));
        let mut at = 0;
        while at < rows {
            let end = (at + chunk).min(rows);
            let wanted: Vec<usize> = (at..end).filter(|r| column.is_present(*r)).collect();
            let got = column.read_rows(store, &wanted).await?;
            for row in at..end {
                // M88: a row the source holds no vector for keeps its slot, with a zero code
                // nothing reads (`search` skips it), so a code's position is still its row.
                if !column.is_present(row) {
                    codes.extend(std::iter::repeat_n(0u8, quantizer.code_len()));
                    continue;
                }
                let v = got
                    .get(&row)
                    .ok_or(ExternalIndexError::Corrupt("a row was not returned"))?;
                let u = unit(v).ok_or(ExternalIndexError::Vector("a row has no direction"))?;
                quantizer.encode(&u)?.write_to(&mut codes);
            }
            at = end;
        }
        let index = Self {
            column,
            quantizer,
            codes,
        };
        store.put(codes_key, index.to_bytes().into()).await?;
        Ok(index)
    }

    /// The codes blob's bytes.
    #[doc(hidden)]
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let c = &self.column;
        let mut b = Vec::with_capacity(96 + self.codes.len());
        b.extend(MAGIC);
        b.extend(c.dims.to_le_bytes());
        b.extend(c.rows.to_le_bytes());
        b.extend((c.key.len() as u32).to_le_bytes());
        b.extend(c.key.as_bytes());
        for n in [c.base, c.stride, c.len] {
            b.extend(n.to_le_bytes());
        }
        put_tag(&mut b, c.tag.as_ref());
        b.extend((c.extents.len() as u32).to_le_bytes());
        for e in &c.extents {
            for n in [e.first_row, e.rows, e.offset, e.compressed] {
                b.extend(n.to_le_bytes());
            }
            b.push(e.codec);
            b.extend(e.plain_len.to_le_bytes());
            match e.dict {
                None => b.push(0),
                Some(d) => {
                    b.push(1);
                    b.extend(d.offset.to_le_bytes());
                    b.extend(d.compressed.to_le_bytes());
                    b.extend(d.count.to_le_bytes());
                    b.push(d.codec);
                }
            }
            b.extend(e.object.to_le_bytes());
        }
        // M87: the further objects a dataset of several data files spans.
        b.extend((c.more.len() as u32).to_le_bytes());
        for o in &c.more {
            b.extend((o.key.len() as u32).to_le_bytes());
            b.extend(o.key.as_bytes());
            b.extend(o.len.to_le_bytes());
            put_tag(&mut b, o.tag.as_ref());
        }
        b.extend(&self.codes);
        b
    }

    /// Reads the codes blob back.
    ///
    /// # Errors
    /// [`ExternalIndexError::Corrupt`] for a blob that is truncated, mislabelled, or whose codes
    /// are not `rows × code_len` bytes; a store failure.
    pub async fn open<S: BlobStore>(
        store: &S,
        codes_key: &Key,
    ) -> Result<Self, ExternalIndexError> {
        let raw = store.get(codes_key).await?;
        Self::from_bytes(&raw)
    }

    /// Parses a codes blob.
    ///
    /// # Errors
    /// [`ExternalIndexError::Corrupt`], as [`Self::open`].
    #[doc(hidden)]
    pub fn from_bytes(raw: &[u8]) -> Result<Self, ExternalIndexError> {
        let bad = ExternalIndexError::Corrupt;
        let mut r = Reader(raw);
        if r.take(4).ok_or(bad("truncated"))? != MAGIC {
            return Err(bad("not an external index"));
        }
        let dims = r.u32().ok_or(bad("truncated"))?;
        let rows = r.u64().ok_or(bad("truncated"))?;
        let klen = r.u32().ok_or(bad("truncated"))? as usize;
        let key = String::from_utf8(r.take(klen).ok_or(bad("truncated"))?.to_vec())
            .map_err(|_| bad("key is not text"))?;
        let base = r.u64().ok_or(bad("truncated"))?;
        let stride = r.u64().ok_or(bad("truncated"))?;
        let len = r.u64().ok_or(bad("truncated"))?;
        let tag = match r.take(1).ok_or(bad("truncated"))?.first() {
            Some(0) => None,
            Some(1) => {
                let n = r.u32().ok_or(bad("truncated"))? as usize;
                let t = String::from_utf8(r.take(n).ok_or(bad("truncated"))?.to_vec())
                    .map_err(|_| bad("tag is not text"))?;
                Some(CasTag::new(t))
            }
            _ => return Err(bad("bad tag flag")),
        };
        let n = r.u32().ok_or(bad("truncated"))? as usize;
        let mut extents = Vec::new();
        for _ in 0..n {
            extents.push(Extent {
                first_row: r.u64().ok_or(bad("truncated"))?,
                rows: r.u64().ok_or(bad("truncated"))?,
                offset: r.u64().ok_or(bad("truncated"))?,
                compressed: r.u64().ok_or(bad("truncated"))?,
                codec: *r
                    .take(1)
                    .ok_or(bad("truncated"))?
                    .first()
                    .ok_or(bad("truncated"))?,
                plain_len: r.u64().ok_or(bad("truncated"))?,
                dict: match r.take(1).ok_or(bad("truncated"))?.first() {
                    Some(0) => None,
                    Some(1) => Some(DictRef {
                        offset: r.u64().ok_or(bad("truncated"))?,
                        compressed: r.u64().ok_or(bad("truncated"))?,
                        count: r.u32().ok_or(bad("truncated"))?,
                        codec: *r
                            .take(1)
                            .ok_or(bad("truncated"))?
                            .first()
                            .ok_or(bad("truncated"))?,
                    }),
                    _ => return Err(bad("bad dictionary flag")),
                },
                object: r.u32().ok_or(bad("truncated"))?,
            });
        }
        let n_more = r.u32().ok_or(bad("truncated"))? as usize;
        let mut more = Vec::new();
        for _ in 0..n_more {
            let klen = r.u32().ok_or(bad("truncated"))? as usize;
            let key = String::from_utf8(r.take(klen).ok_or(bad("truncated"))?.to_vec())
                .map_err(|_| bad("key is not text"))?;
            let len = r.u64().ok_or(bad("truncated"))?;
            let tag = match r.take(1).ok_or(bad("truncated"))?.first() {
                Some(0) => None,
                Some(1) => {
                    let n = r.u32().ok_or(bad("truncated"))? as usize;
                    let t = String::from_utf8(r.take(n).ok_or(bad("truncated"))?.to_vec())
                        .map_err(|_| bad("tag is not text"))?;
                    Some(CasTag::new(t))
                }
                _ => return Err(bad("bad tag flag")),
            };
            more.push(ObjectRef { key, len, tag });
        }
        let quantizer = Quantizer::new(dims as usize);
        let want = usize::try_from(rows)
            .ok()
            .and_then(|n| n.checked_mul(quantizer.code_len()))
            .ok_or(bad("rows overflow"))?;
        let codes = r.rest();
        if dims == 0 || codes.len() != want {
            return Err(bad("codes are not rows × code length"));
        }
        if !extents.is_empty() {
            let mut objects = vec![ObjectRef {
                key: key.clone(),
                len,
                tag: tag.clone(),
            }];
            objects.extend(more.iter().cloned());
            ExternalColumn::from_objects(objects, dims, rows, extents.clone())
                .map_err(|_| bad("the runs do not fit the objects"))?;
        }
        Ok(Self {
            column: ExternalColumn {
                key,
                base,
                dims,
                stride,
                rows,
                len,
                tag,
                extents,
                more,
            },
            quantizer,
            codes: codes.to_vec(),
        })
    }

    /// Runs the pinned column is made of: 0 for a single run.
    #[doc(hidden)]
    #[must_use]
    pub fn extent_count(&self) -> usize {
        self.column.extents.len()
    }

    /// This index's codes over another column (a test's control: right codes, wrong rows).
    #[doc(hidden)]
    #[must_use]
    pub fn with_column(mut self, column: ExternalColumn) -> Self {
        self.column = column;
        self
    }

    /// The `k` rows nearest `query` by cosine, as `(row, exact cosine)` descending, ties by row.
    /// The `window` rows with the best code estimates are read from the customer's object.
    ///
    /// # Errors
    /// A zero or wrong-width query; a changed source ([`ExternalError::Changed`]); a store
    /// failure.
    pub async fn search<S: BlobStore>(
        &self,
        store: &S,
        query: &[f32],
        k: usize,
        window: usize,
    ) -> Result<Vec<(usize, f32)>, ExternalIndexError> {
        self.search_where(store, query, k, window, &|_| true).await
    }

    /// [`Self::search`], over only the rows `allow` admits (M73): the others are never scored,
    /// read or returned, so a filter cannot leave the window full of rows it then drops.
    ///
    /// # Errors
    /// As [`Self::search`].
    pub async fn search_where<S: BlobStore>(
        &self,
        store: &S,
        query: &[f32],
        k: usize,
        window: usize,
        allow: &dyn Fn(usize) -> bool,
    ) -> Result<Vec<(usize, f32)>, ExternalIndexError> {
        if query.len() != self.column.dims as usize {
            return Err(ExternalIndexError::Vector("wrong width"));
        }
        let q = unit(query).ok_or(ExternalIndexError::Vector("the query has no direction"))?;
        let prepared = self.quantizer.prepare(&q)?;
        let step = self.quantizer.code_len();
        let mut scored: Vec<(usize, f32)> = self
            .codes
            .chunks_exact(step)
            .enumerate()
            .filter_map(|(row, raw)| {
                // A null row (M88) has a slot and no vector.
                if !allow(row) || !self.column.is_present(row) {
                    return None;
                }
                let code = self.quantizer.read_code(raw)?;
                Some((row, self.quantizer.estimate_prepared(&code, &prepared)))
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.truncate(window.max(k));
        let rows: Vec<usize> = scored.iter().map(|(r, _)| *r).collect();
        let got = self.column.read_rows(store, &rows).await?;
        let mut exact: Vec<(usize, f32)> = got
            .iter()
            .filter_map(|(row, v)| {
                let u = unit(v)?;
                Some((*row, u.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>()))
            })
            .collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        exact.truncate(k);
        Ok(exact)
    }
}

/// A CAS tag as a presence byte and, when present, a length-prefixed string.
fn put_tag(b: &mut Vec<u8>, tag: Option<&CasTag>) {
    match tag {
        None => b.push(0),
        Some(t) => {
            b.push(1);
            b.extend((t.as_str().len() as u32).to_le_bytes());
            b.extend(t.as_str().as_bytes());
        }
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }
}
