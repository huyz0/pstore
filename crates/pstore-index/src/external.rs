//! An index over vectors left in the customer's object (M69, OQ-169).
//!
//! The index is **one blob of 1-bit codes** and the pin of the column they were taken from. No
//! float of the customer's data is written. A search scores every code, keeps the `window`
//! best, and reads only those rows out of the customer's object -- through
//! [`ExternalColumn`], which refuses an object that changed -- to return the exact cosine of
//! the top `k`.

use crate::rabitq::{QuantError, Quantizer};
use pstore_blob::{BlobError, BlobStore, Key};
use pstore_format::external::{
    AttrValue, DictRef, Extent, ExternalAttribute, ExternalColumn, ExternalError, ObjectRef,
};
use pstore_types::CasTag;

const MAGIC: &[u8; 4] = b"PXI6";
/// A blob with the cluster section after its codes (M96).
const MAGIC_CLUSTERED: &[u8; 4] = b"PXI7";
/// Rows read to find the centroids of a clustered build.
pub const CLUSTER_SAMPLE: usize = 8_192;

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
    /// The rows grouped by nearest centroid, if the build clustered them (M96).
    clusters: Option<Clusters>,
}

/// Centroids and each row's list (M96). Codes stay in row order; this only says which rows a
/// probe scores.
#[derive(Debug, Clone)]
struct Clusters {
    /// `nlist × dims` floats, a centroid after another.
    centroids: Vec<f32>,
    /// The list of each row (`rows` entries; a null row's is 0 and never read).
    assign: Vec<u32>,
    /// Each list's rows, ascending: derived from `assign`.
    lists: Vec<Vec<u32>>,
}

impl Clusters {
    fn new(centroids: Vec<f32>, assign: Vec<u32>, nlist: usize) -> Option<Self> {
        let mut lists = vec![Vec::new(); nlist];
        for (row, list) in assign.iter().enumerate() {
            lists
                .get_mut(*list as usize)?
                .push(u32::try_from(row).ok()?);
        }
        Some(Self {
            centroids,
            assign,
            lists,
        })
    }

    /// The `probes` lists whose centroids are nearest the unit vector `q`, nearest first.
    fn nearest(&self, q: &[f32], probes: usize) -> Vec<usize> {
        let dims = q.len();
        let mut dist: Vec<(usize, f32)> = self
            .centroids
            .chunks_exact(dims)
            .enumerate()
            .map(|(i, c)| (i, sq_dist(c, q)))
            .collect();
        crate::top::top_k_by(&mut dist, probes, |a, b| {
            a.1.total_cmp(&b.1).then(a.0.cmp(&b.0))
        });
        dist.into_iter().map(|(i, _)| i).collect()
    }
}

/// At most about `cap` of the present rows of `0..rows`, evenly strided over the **present**
/// ones (so nulls that fall on the stride cannot empty the sample).
fn sample_rows(rows: usize, cap: usize, present: impl Fn(usize) -> bool) -> Vec<usize> {
    let all: Vec<usize> = (0..rows).filter(|r| present(*r)).collect();
    let step = all.len().div_ceil(cap.max(1)).max(1);
    all.into_iter().step_by(step).collect()
}

fn sq_dist(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
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
        Self::build_with(store, codes_key, column, chunk, None).await
    }

    /// [`Self::build`], and the rows grouped into about `lists` lists by nearest centroid
    /// (M96), so [`Self::search_probes`] scores only some of them. The centroids are found
    /// from a strided sample of at most [`CLUSTER_SAMPLE`] rows.
    ///
    /// # Errors
    /// As [`Self::build`]; `lists` of 0; a column with no present row.
    pub async fn build_clustered<S: BlobStore>(
        store: &S,
        codes_key: &Key,
        column: ExternalColumn,
        chunk: usize,
        lists: usize,
    ) -> Result<Self, ExternalIndexError> {
        if lists == 0 {
            return Err(ExternalIndexError::Corrupt("lists is zero"));
        }
        let rows = usize::try_from(column.rows)
            .map_err(|_| ExternalIndexError::Corrupt("too many rows"))?;
        let wanted = sample_rows(rows, CLUSTER_SAMPLE, |r| column.is_present(r));
        if wanted.is_empty() {
            return Err(ExternalIndexError::Corrupt("no row to cluster"));
        }
        let mut sample = Vec::with_capacity(wanted.len());
        for part in wanted.chunks(chunk.max(1)) {
            let got = column.read_rows(store, part).await?;
            for row in part {
                let v = got
                    .get(row)
                    .ok_or(ExternalIndexError::Corrupt("a row was not returned"))?;
                sample.push(unit(v).ok_or(ExternalIndexError::Vector("a row has no direction"))?);
            }
        }
        let params = crate::cluster::Params {
            target_list_size: sample.len().div_ceil(lists).max(1),
            replicas: 0,
            boundary: 0.0,
            ..crate::cluster::Params::default()
        };
        let found = crate::cluster::Clustering::build(&sample, params);
        Self::build_with(store, codes_key, column, chunk, Some(found.centroids())).await
    }

    async fn build_with<S: BlobStore>(
        store: &S,
        codes_key: &Key,
        column: ExternalColumn,
        chunk: usize,
        centroids: Option<&[Vec<f32>]>,
    ) -> Result<Self, ExternalIndexError> {
        let dims = column.dims as usize;
        let quantizer = Quantizer::new(dims);
        let rows = usize::try_from(column.rows)
            .map_err(|_| ExternalIndexError::Corrupt("too many rows"))?;
        let chunk = chunk.max(1);
        let mut codes = Vec::with_capacity(rows.saturating_mul(quantizer.code_len()));
        let mut assign: Vec<u32> = Vec::new();
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
                    if centroids.is_some() {
                        assign.push(0);
                    }
                    continue;
                }
                let v = got
                    .get(&row)
                    .ok_or(ExternalIndexError::Corrupt("a row was not returned"))?;
                let u = unit(v).ok_or(ExternalIndexError::Vector("a row has no direction"))?;
                quantizer.encode(&u)?.write_to(&mut codes);
                if let Some(cs) = centroids {
                    let nearest = cs
                        .iter()
                        .enumerate()
                        .map(|(i, c)| (i, sq_dist(c, &u)))
                        .min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)))
                        .map_or(0, |(i, _)| i);
                    assign.push(u32::try_from(nearest).unwrap_or(0));
                }
            }
            at = end;
        }
        let clusters = match centroids {
            Some(cs) => Some(
                Clusters::new(cs.iter().flatten().copied().collect(), assign, cs.len())
                    .ok_or(ExternalIndexError::Corrupt("a list id is out of range"))?,
            ),
            None => None,
        };
        let index = Self {
            column,
            quantizer,
            codes,
            clusters,
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
        b.extend(if self.clusters.is_some() {
            MAGIC_CLUSTERED
        } else {
            MAGIC
        });
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
        if let Some(cl) = &self.clusters {
            b.extend((cl.lists.len() as u32).to_le_bytes());
            for x in &cl.centroids {
                b.extend(x.to_le_bytes());
            }
            for l in &cl.assign {
                b.extend(l.to_le_bytes());
            }
        }
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
        let clustered = match r.take(4).ok_or(bad("truncated"))? {
            m if m == MAGIC => false,
            m if m == MAGIC_CLUSTERED => true,
            _ => return Err(bad("not an external index")),
        };
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
        let rest = r.rest();
        let (codes, tail) = rest
            .split_at_checked(want)
            .ok_or(bad("codes are not rows × code length"))?;
        if dims == 0 {
            return Err(bad("codes are not rows × code length"));
        }
        let clusters = if clustered {
            let mut t = Reader(tail);
            let nlist = t.u32().ok_or(bad("truncated"))? as usize;
            let nrows = rows as usize;
            let cfloats = nlist
                .checked_mul(dims as usize)
                .ok_or(bad("clusters overflow"))?;
            let cbytes = t
                .take(cfloats.checked_mul(4).ok_or(bad("clusters overflow"))?)
                .ok_or(bad("truncated"))?;
            let abytes = t
                .take(nrows.checked_mul(4).ok_or(bad("clusters overflow"))?)
                .ok_or(bad("truncated"))?;
            if !t.rest().is_empty() || nlist == 0 {
                return Err(bad("the cluster section is not nlist × dims + rows"));
            }
            let f = |c: &[u8]| c.try_into().map_or(0, u32::from_le_bytes);
            let centroids = cbytes
                .chunks_exact(4)
                .map(|c| f32::from_bits(f(c)))
                .collect();
            let assign = abytes.chunks_exact(4).map(f).collect();
            Some(Clusters::new(centroids, assign, nlist).ok_or(bad("a list id is out of range"))?)
        } else {
            if !tail.is_empty() {
                return Err(bad("codes are not rows × code length"));
            }
            None
        };
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
            clusters,
        })
    }

    /// Lists the build clustered the rows into: 0 for a flat index.
    #[doc(hidden)]
    #[must_use]
    pub fn cluster_count(&self) -> usize {
        self.clusters.as_ref().map_or(0, |c| c.lists.len())
    }

    /// The rows -- vectors present -- of the `probes` lists nearest `query`, ascending: what a
    /// probing search scores.
    ///
    /// # Errors
    /// A zero or wrong-width query, `probes` of 0, or an index with no clusters.
    #[doc(hidden)]
    pub fn probed_rows(
        &self,
        query: &[f32],
        probes: usize,
    ) -> Result<Vec<usize>, ExternalIndexError> {
        let cl = self
            .clusters
            .as_ref()
            .ok_or(ExternalIndexError::Corrupt("the index has no clusters"))?;
        if probes == 0 {
            return Err(ExternalIndexError::Corrupt("probes is zero"));
        }
        if query.len() != self.column.dims as usize {
            return Err(ExternalIndexError::Vector("wrong width"));
        }
        let q = unit(query).ok_or(ExternalIndexError::Vector("the query has no direction"))?;
        let mut rows: Vec<usize> = cl
            .nearest(&q, probes)
            .into_iter()
            .filter_map(|l| cl.lists.get(l))
            .flatten()
            .map(|r| *r as usize)
            .filter(|r| self.column.is_present(*r))
            .collect();
        rows.sort_unstable();
        Ok(rows)
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

    /// [`Self::search_where`], over the rows whose value in `attr` -- a scalar column of **this
    /// index's own object** (M95) -- `keep` admits; `keep` sees `None` for a null. The column is
    /// read once, in full, before any code is scored.
    ///
    /// # Errors
    /// As [`Self::search`]; [`ExternalIndexError::Corrupt`] for an attribute in another object or
    /// of another row count, before any vector is read.
    pub async fn search_attr<S: BlobStore>(
        &self,
        store: &S,
        query: &[f32],
        k: usize,
        window: usize,
        attr: &ExternalAttribute,
        keep: &(dyn Fn(Option<&AttrValue>) -> bool + Sync),
    ) -> Result<Vec<(usize, f32)>, ExternalIndexError> {
        if attr.keys() != self.column.keys() {
            return Err(ExternalIndexError::Corrupt(
                "the attribute is in another object than the vectors",
            ));
        }
        if attr.rows() != self.column.rows {
            return Err(ExternalIndexError::Corrupt(
                "the attribute has another number of rows than the vectors",
            ));
        }
        let mask: Vec<bool> = attr
            .read_all(store)
            .await?
            .iter()
            .map(|v| keep(v.as_ref()))
            .collect();
        self.search_where(store, query, k, window, &|row| {
            mask.get(row).copied().unwrap_or(false)
        })
        .await
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
        allow: &(dyn Fn(usize) -> bool + Sync),
    ) -> Result<Vec<(usize, f32)>, ExternalIndexError> {
        self.search_inner(store, query, k, window, allow, None)
            .await
    }

    /// [`Self::search`], scoring only the rows of the `probes` lists nearest the query (M96).
    /// `probes` of at least [`Self::cluster_count`] scores every row, as `search` does.
    ///
    /// # Errors
    /// As [`Self::search`]; `probes` of 0; an index built without clusters.
    pub async fn search_probes<S: BlobStore>(
        &self,
        store: &S,
        query: &[f32],
        k: usize,
        window: usize,
        probes: usize,
    ) -> Result<Vec<(usize, f32)>, ExternalIndexError> {
        self.search_inner(store, query, k, window, &|_| true, Some(probes))
            .await
    }

    /// [`Self::search_where`] and [`Self::search_probes`] together.
    ///
    /// # Errors
    /// As [`Self::search_probes`].
    pub async fn search_where_probes<S: BlobStore>(
        &self,
        store: &S,
        query: &[f32],
        k: usize,
        window: usize,
        allow: &(dyn Fn(usize) -> bool + Sync),
        probes: usize,
    ) -> Result<Vec<(usize, f32)>, ExternalIndexError> {
        self.search_inner(store, query, k, window, allow, Some(probes))
            .await
    }

    async fn search_inner<S: BlobStore>(
        &self,
        store: &S,
        query: &[f32],
        k: usize,
        window: usize,
        allow: &(dyn Fn(usize) -> bool + Sync),
        probes: Option<usize>,
    ) -> Result<Vec<(usize, f32)>, ExternalIndexError> {
        if query.len() != self.column.dims as usize {
            return Err(ExternalIndexError::Vector("wrong width"));
        }
        let q = unit(query).ok_or(ExternalIndexError::Vector("the query has no direction"))?;
        let prepared = self.quantizer.prepare(&q)?;
        let step = self.quantizer.code_len();
        let score = |row: usize| -> Option<(usize, f32)> {
            // A null row (M88) has a slot and no vector.
            if !allow(row) || !self.column.is_present(row) {
                return None;
            }
            let raw = self.codes.get(row * step..(row + 1) * step)?;
            Some((row, self.quantizer.estimate_prepared_raw(raw, &prepared)?))
        };
        let mut scored: Vec<(usize, f32)> = match probes {
            None => (0..self.codes.len() / step).filter_map(score).collect(),
            Some(p) => {
                let cl = self
                    .clusters
                    .as_ref()
                    .ok_or(ExternalIndexError::Corrupt("the index has no clusters"))?;
                if p == 0 {
                    return Err(ExternalIndexError::Corrupt("probes is zero"));
                }
                cl.nearest(&q, p)
                    .into_iter()
                    .filter_map(|l| cl.lists.get(l))
                    .flatten()
                    .filter_map(|r| score(*r as usize))
                    .collect()
            }
        };
        crate::top::top_k_by(&mut scored, window.max(k), crate::top::best_first);
        let rows: Vec<usize> = scored.iter().map(|(r, _)| *r).collect();
        let got = self.column.read_rows(store, &rows).await?;
        let mut exact: Vec<(usize, f32)> = got
            .iter()
            .filter_map(|(row, v)| {
                let u = unit(v)?;
                Some((*row, u.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>()))
            })
            .collect();
        crate::top::top_k_by(&mut exact, k, crate::top::best_first);
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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::panic,
        reason = "assertions in tests are the reporting mechanism"
    )]
    use super::{ExternalIndex, ExternalIndexError, MAGIC, MAGIC_CLUSTERED, sample_rows};
    use crate::rabitq::{QuantError, Quantizer};

    const DIMS: u32 = 8;
    const ROWS: u64 = 2;

    /// A codes blob written by hand: `ROWS` rows of `DIMS`, no tag, `extents` as given, then
    /// `codes` bytes of codes and `tail`.
    fn blob(
        magic: &[u8; 4],
        dims: u32,
        tag: &[u8],
        extents: &[&[u8]],
        codes: usize,
        tail: &[u8],
    ) -> Vec<u8> {
        let mut b = magic.to_vec();
        b.extend(dims.to_le_bytes());
        b.extend(ROWS.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.push(b'k');
        for n in [0u64, 32, 64] {
            b.extend(n.to_le_bytes());
        }
        b.extend(tag);
        b.extend((extents.len() as u32).to_le_bytes());
        for e in extents {
            b.extend(*e);
        }
        b.extend(0u32.to_le_bytes());
        b.extend(vec![0u8; codes]);
        b.extend(tail);
        b
    }

    /// One extent, its dictionary flag `dict` followed by nothing.
    fn extent(dict: u8) -> Vec<u8> {
        let mut e = Vec::new();
        for n in [0u64, ROWS, 0, 64] {
            e.extend(n.to_le_bytes());
        }
        e.push(0);
        e.extend(64u64.to_le_bytes());
        e.push(dict);
        e.extend(0u32.to_le_bytes());
        e
    }

    fn refusal(raw: &[u8]) -> &'static str {
        match ExternalIndex::from_bytes(raw) {
            Err(ExternalIndexError::Corrupt(why)) => why,
            other => panic!("not refused as corrupt: {other:?}"),
        }
    }

    #[test]
    fn a_damaged_codes_blob_is_refused_by_name() {
        let code = Quantizer::new(DIMS as usize).code_len() * ROWS as usize;
        // The control: the same bytes, well formed, open.
        let good = ExternalIndex::from_bytes(&blob(MAGIC, DIMS, &[0], &[], code, &[])).unwrap();
        assert_eq!(good.cluster_count(), 0);
        let tagged = blob(MAGIC, DIMS, &[1, 1, 0, 0, 0, b't'], &[], code, &[]);
        assert!(ExternalIndex::from_bytes(&tagged).is_ok());

        assert_eq!(
            refusal(&blob(MAGIC, DIMS, &[2], &[], code, &[])),
            "bad tag flag"
        );
        assert_eq!(
            refusal(&blob(MAGIC, DIMS, &[0], &[&extent(2)], code, &[])),
            "bad dictionary flag"
        );
        let short = "codes are not rows × code length";
        assert_eq!(refusal(&blob(MAGIC, DIMS, &[0], &[], code - 1, &[])), short);
        assert_eq!(refusal(&blob(MAGIC, DIMS, &[0], &[], code, &[0])), short);
        assert_eq!(refusal(&blob(MAGIC, 0, &[0], &[], 0, &[])), short);
        // No rows and no width: zero codes is the right length, and still not an index.
        let mut empty = blob(MAGIC, 0, &[0], &[], 0, &[]);
        empty[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(refusal(&empty), short);
        assert_eq!(refusal(b"PXI5"), "not an external index");
    }

    /// Every section a codes blob can carry: a tag, a run with a dictionary and one without,
    /// a further object with a tag, codes, and the cluster section. `object` is the object the
    /// second run names.
    fn full(object: u32) -> Vec<u8> {
        let tag = [1, 1, 0, 0, 0, b't'];
        let run = |first: u64, dict: bool, object: u32| {
            let mut e = Vec::new();
            for n in [first, 1, 0, 64] {
                e.extend(n.to_le_bytes());
            }
            e.push(0);
            e.extend(64u64.to_le_bytes());
            if dict {
                e.push(1);
                e.extend(0u64.to_le_bytes());
                e.extend(8u64.to_le_bytes());
                e.extend(1u32.to_le_bytes());
                e.push(0);
            } else {
                e.push(0);
            }
            e.extend(object.to_le_bytes());
            e
        };
        let mut b = MAGIC_CLUSTERED.to_vec();
        b.extend(DIMS.to_le_bytes());
        b.extend(ROWS.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.push(b'k');
        for n in [0u64, 32, 64] {
            b.extend(n.to_le_bytes());
        }
        b.extend(tag);
        b.extend(2u32.to_le_bytes());
        b.extend(run(0, true, 0));
        b.extend(run(1, false, object));
        b.extend(1u32.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.push(b'm');
        b.extend(64u64.to_le_bytes());
        b.extend(tag);
        b.extend(vec![
            0u8;
            Quantizer::new(DIMS as usize).code_len() * ROWS as usize
        ]);
        b.extend(1u32.to_le_bytes());
        for i in 0..DIMS {
            b.extend(if i == 0 { 1f32 } else { 0f32 }.to_le_bytes());
        }
        for _ in 0..ROWS {
            b.extend(0u32.to_le_bytes());
        }
        b
    }

    #[test]
    fn every_truncation_of_a_full_codes_blob_is_refused() {
        let raw = full(1);
        let ix = ExternalIndex::from_bytes(&raw).unwrap();
        assert_eq!((ix.cluster_count(), ix.extent_count()), (1, 2));
        // The blob reads back as the bytes it was read from.
        assert_eq!(ix.to_bytes(), raw);
        for n in 0..raw.len() {
            assert!(
                ExternalIndex::from_bytes(&raw[..n]).is_err(),
                "a blob cut to {n} of {} bytes was read",
                raw.len()
            );
        }
        assert_eq!(refusal(&full(2)), "the runs do not fit the objects");
        // A list id past the lists, and a cluster section with no list.
        let mut far = raw.clone();
        let at = far.len() - 4;
        far[at..].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(refusal(&far), "a list id is out of range");
        let mut more = raw;
        more.push(0);
        assert_eq!(
            refusal(&more),
            "the cluster section is not nlist × dims + rows"
        );
    }

    #[test]
    fn probing_refuses_no_probes_and_a_query_of_another_width() {
        let code = Quantizer::new(DIMS as usize).code_len() * ROWS as usize;
        // One list, its centroid on the first axis, both rows in it.
        let mut tail = 1u32.to_le_bytes().to_vec();
        for i in 0..DIMS {
            tail.extend(if i == 0 { 1f32 } else { 0f32 }.to_le_bytes());
        }
        for _ in 0..ROWS {
            tail.extend(0u32.to_le_bytes());
        }
        let ix = ExternalIndex::from_bytes(&blob(MAGIC_CLUSTERED, DIMS, &[0], &[], code, &tail))
            .unwrap();
        let q = [1f32, 0., 0., 0., 0., 0., 0., 0.];
        assert_eq!(ix.probed_rows(&q, 1).unwrap(), [0, 1]);
        assert!(matches!(
            ix.probed_rows(&q, 0),
            Err(ExternalIndexError::Corrupt("probes is zero"))
        ));
        assert!(matches!(
            ix.probed_rows(&q[..7], 1),
            Err(ExternalIndexError::Vector("wrong width"))
        ));
        let quant = QuantError::Dimension {
            expected: 8,
            got: 7,
        };
        assert!(matches!(
            ExternalIndexError::from(quant),
            ExternalIndexError::Vector("wrong width")
        ));
    }

    #[test]
    fn nulls_on_the_stride_cannot_empty_the_sample() {
        // 20,000 rows, every third a null, a cap that strides by 3 over the rows.
        let got = sample_rows(20_000, 8_192, |r| r % 3 != 0);
        assert!(!got.is_empty());
        assert!(got.iter().all(|r| r % 3 != 0));
        assert!(got.len() <= 8_192 && got.len() > 4_096, "{}", got.len());
        // Every present row when they fit; none past the end; ascending.
        assert_eq!(
            sample_rows(10, 100, |r| r != 4),
            [0, 1, 2, 3, 5, 6, 7, 8, 9]
        );
        assert!(sample_rows(10, 100, |_| false).is_empty());
        assert!(got.is_sorted());
    }
}
