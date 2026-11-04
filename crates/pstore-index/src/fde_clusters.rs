//! The multi-vector encoding, clustered by a permutation sidecar (M71).
//!
//! A segment has one row order, which the dense clustering decides and the postings share, so
//! the encoded field cannot be reordered into lists. This object holds the encoded rows'
//! centroids and, per list, the **segment row numbers** it holds; a search reads only the best
//! lists' rows of the encoded field, by computed range.

use crate::cluster::{Clustering, Params};
use pstore_blob::{BlobStore, Key};
use pstore_format::FormatError;

const MAGIC: &[u8; 4] = b"PFC1";

/// The sidecar's derived key.
#[must_use]
pub fn key(segment: &Key) -> Key {
    Key::new(format!("{}.fdec", segment.as_str()))
}

/// The encoded rows' derived key: the vectors, in list order.
#[must_use]
pub fn vectors_key(segment: &Key) -> Key {
    Key::new(format!("{}.fdev", segment.as_str()))
}

/// The name a named dense field's clusters carry in the set (M83). It starts with a NUL, which
/// no field name may, so it can never be a multi-vector field's name.
#[must_use]
pub fn dense_name(field: &str) -> String {
    format!("\0dense\0{field}")
}

/// Centroids of a segment's encoded rows, and the rows of each list.
#[derive(Debug, Clone, PartialEq)]
pub struct FdeClusters {
    field: String,
    centroids: Vec<Vec<f32>>,
    lists: Vec<Vec<u32>>,
    rows: u32,
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl FdeClusters {
    /// Clusters the encoded rows, one `Vec<f32>` a row in segment order, and lays them out in
    /// list order: the second value is the `.fdev` blob.
    #[must_use]
    pub fn build(field: &str, rows: &[Vec<f32>], params: Params) -> (Self, Vec<u8>) {
        let c = Clustering::build(rows, params);
        let mut blob = Vec::new();
        for list in c.lists() {
            for r in list {
                for x in rows.get(*r).map_or(&[][..], Vec::as_slice) {
                    blob.extend(x.to_le_bytes());
                }
            }
        }
        let me = Self {
            field: field.to_owned(),
            centroids: c.centroids().to_vec(),
            lists: c
                .lists()
                .iter()
                .map(|l| l.iter().filter_map(|r| u32::try_from(*r).ok()).collect())
                .collect(),
            rows: u32::try_from(rows.len()).unwrap_or(u32::MAX),
        };
        (me, blob)
    }

    /// Writes both objects.
    ///
    /// # Errors
    /// A store failure.
    pub async fn put<S: BlobStore>(
        &self,
        store: &S,
        segment_key: &Key,
        vectors: Vec<u8>,
    ) -> Result<(), pstore_blob::BlobError> {
        store.put(&vectors_key(segment_key), vectors.into()).await?;
        store.put(&key(segment_key), self.encode().into()).await?;
        Ok(())
    }

    /// The multi-vector field these lists cluster the encoding of.
    #[must_use]
    pub fn field(&self) -> &str {
        &self.field
    }

    /// Lists in the sidecar.
    #[must_use]
    pub fn lists(&self) -> usize {
        self.lists.len()
    }

    /// The rows list `i` holds (0 for a list that does not exist).
    #[must_use]
    pub fn list_len(&self, i: usize) -> usize {
        self.lists.get(i).map_or(0, Vec::len)
    }

    /// The lists in the order a query `q` (in the clustered space) would probe them: best
    /// centroid first, ties by list number.
    #[must_use]
    pub fn probe_order(&self, q: &[f32]) -> Vec<usize> {
        let mut order: Vec<(usize, f32)> = self
            .centroids
            .iter()
            .enumerate()
            .map(|(i, c)| (i, dot(q, c)))
            .collect();
        order.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        order.into_iter().map(|(i, _)| i).collect()
    }

    /// The sidecar's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let dim = self.centroids.first().map_or(0, Vec::len);
        let mut b = Vec::new();
        b.extend(MAGIC);
        b.extend(self.rows.to_le_bytes());
        b.extend((self.field.len() as u32).to_le_bytes());
        b.extend(self.field.as_bytes());
        b.extend((self.centroids.len() as u32).to_le_bytes());
        b.extend((dim as u32).to_le_bytes());
        for c in &self.centroids {
            for x in c {
                b.extend(x.to_le_bytes());
            }
        }
        for l in &self.lists {
            b.extend((l.len() as u32).to_le_bytes());
            for r in l {
                b.extend(r.to_le_bytes());
            }
        }
        b
    }

    /// Reads a sidecar back, or `None` for anything truncated, mislabelled, ragged or naming a
    /// row past the segment's end: a wrong list would be a wrong answer, not an error.
    #[must_use]
    pub fn decode(raw: &[u8]) -> Option<Self> {
        let mut at = 0usize;
        let mut take = |n: usize| -> Option<&[u8]> {
            let s = raw.get(at..at.checked_add(n)?)?;
            at += n;
            Some(s)
        };
        let u32_of = |s: &[u8]| -> Option<u32> { Some(u32::from_le_bytes(s.try_into().ok()?)) };
        if take(4)? != MAGIC {
            return None;
        }
        let rows = u32_of(take(4)?)?;
        let flen = u32_of(take(4)?)? as usize;
        let field = String::from_utf8(take(flen)?.to_vec()).ok()?;
        let n = u32_of(take(4)?)? as usize;
        let dim = u32_of(take(4)?)? as usize;
        // ⚠️ No `with_capacity(n)`: `n` is read from the blob, and a damaged one would ask for
        // gigabytes before the first `take` could refuse it.
        let mut centroids = Vec::new();
        for _ in 0..n {
            let s = take(dim.checked_mul(4)?)?;
            centroids.push(
                s.chunks_exact(4)
                    .map(|b| f32::from_le_bytes(b.try_into().unwrap_or([0; 4])))
                    .collect(),
            );
        }
        let mut lists = Vec::new();
        for _ in 0..n {
            let len = u32_of(take(4)?)? as usize;
            let s = take(len.checked_mul(4)?)?;
            let l: Vec<u32> = s
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap_or([0; 4])))
                .collect();
            if l.iter().any(|r| *r >= rows) {
                return None;
            }
            lists.push(l);
        }
        (at == raw.len()).then_some(Self {
            field,
            centroids,
            lists,
            rows,
        })
    }

    /// Reads the sidecar for `segment_key`.
    ///
    /// # Errors
    /// A store failure, or [`FormatError::Corrupt`] for a damaged sidecar.
    pub async fn open<S: BlobStore>(store: &S, segment_key: &Key) -> Result<Self, FormatError> {
        let raw = store
            .get(&key(segment_key))
            .await
            .map_err(|_| FormatError::Truncated)?;
        Self::decode(&raw).ok_or(FormatError::Corrupt("damaged multi-vector cluster sidecar"))
    }

    /// The top `limit` rows by encoded dot product, over the rows of the `probes` best lists.
    ///
    /// # Errors
    /// [`FormatError::Corrupt`] if `.fdev` is not the size the lists imply; a store failure.
    pub async fn search<S: BlobStore>(
        &self,
        store: &S,
        segment_key: &Key,
        query: &[Vec<f32>],
        probes: usize,
        limit: usize,
    ) -> Result<Vec<(usize, f32)>, FormatError> {
        self.search_at(store, segment_key, 0, query, probes, limit)
            .await
    }

    /// [`Self::search`], with this field's vectors starting `base` bytes into `.fdev` (M74: a
    /// segment's fields share the one object).
    ///
    /// # Errors
    /// As [`Self::search`].
    pub async fn search_at<S: BlobStore>(
        &self,
        store: &S,
        segment_key: &Key,
        base: u64,
        query: &[Vec<f32>],
        probes: usize,
        limit: usize,
    ) -> Result<Vec<(usize, f32)>, FormatError> {
        let q = pstore_format::fde::encode_query(&pstore_format::fde::DEFAULT, query);
        self.search_vector_at(store, segment_key, base, &q, probes, limit)
            .await
    }

    /// The top `limit` rows by dot product with `q`, over the rows of the `probes` best lists,
    /// with this field's vectors `base` bytes into `.fdev`. `q` is already in the clustered
    /// space: an encoded multi-vector query, or (M83) a named dense field's transformed query.
    ///
    /// # Errors
    /// As [`Self::search`].
    pub async fn search_vector_at<S: BlobStore>(
        &self,
        store: &S,
        segment_key: &Key,
        base: u64,
        q: &[f32],
        probes: usize,
        limit: usize,
    ) -> Result<Vec<(usize, f32)>, FormatError> {
        Ok(self
            .search_vector_probed(store, segment_key, base, q, probes, limit)
            .await?
            .0)
    }

    /// [`Self::search_vector_at`], with the lists it probed (M165).
    ///
    /// # Errors
    /// As [`Self::search`].
    pub async fn search_vector_probed<S: BlobStore>(
        &self,
        store: &S,
        segment_key: &Key,
        base: u64,
        q: &[f32],
        probes: usize,
        limit: usize,
    ) -> Result<(Vec<(usize, f32)>, usize), FormatError> {
        let dim = self.centroids.first().map_or(0, Vec::len);
        let width = (dim * 4) as u64;
        let order = self.probe_order(q);
        // Where each list starts, in rows of `.fdev`.
        let mut starts = Vec::with_capacity(self.lists.len());
        let mut at = 0u64;
        for l in &self.lists {
            starts.push(at);
            at += l.len() as u64;
        }
        let chosen: Vec<usize> = order.iter().take(probes.max(1)).copied().collect();
        let ranges: Vec<std::ops::Range<u64>> = chosen
            .iter()
            .filter_map(|i| {
                let lo = base + *starts.get(*i)? * width;
                Some(lo..lo + self.lists.get(*i)?.len() as u64 * width)
            })
            .collect();
        let probed = ranges.len();
        let bufs = store.get_ranges(&vectors_key(segment_key), &ranges).await?;
        // Indexed by row, first score kept, as the `BTreeMap`'s `or_insert` was (M119); a row
        // past any real segment's -- a corrupt sidecar's -- in a map beside it, as before.
        let mut best: Vec<Option<f32>> = Vec::new();
        let mut far: std::collections::BTreeMap<usize, f32> = std::collections::BTreeMap::new();
        for (i, raw) in chosen.iter().zip(&bufs) {
            let list = self.lists.get(*i).ok_or(FormatError::Corrupt("list"))?;
            if raw.len() != list.len() * dim * 4 {
                return Err(FormatError::Corrupt(
                    "vector blob is not the size the lists imply",
                ));
            }
            for (row, v) in list.iter().zip(raw.chunks_exact(dim.max(1) * 4)) {
                let score: f32 = v
                    .chunks_exact(4)
                    .zip(q)
                    .map(|(b, x)| f32::from_le_bytes(b.try_into().unwrap_or([0; 4])) * x)
                    .sum();
                let at = *row as usize;
                if at >= crate::top::DENSE_ROWS {
                    far.entry(at).or_insert(score);
                    continue;
                }
                if best.len() <= at {
                    best.resize(at + 1, None);
                }
                if let Some(slot) = best.get_mut(at) {
                    slot.get_or_insert(score);
                }
            }
        }
        let mut scored: Vec<(usize, f32)> = best
            .into_iter()
            .enumerate()
            .filter_map(|(row, s)| Some((row, s?)))
            .chain(far)
            .collect();
        crate::top::top_k_by(&mut scored, limit, crate::top::best_first);
        Ok((scored, probed))
    }
}

const SET_MAGIC: &[u8; 4] = b"PFS1";

/// The clusters of every multi-vector field a segment carries, in the one `.fdec` and `.fdev`
/// pair (M74). One pair, not one a field: GC and replication derive sidecar keys from the
/// segment's, and a LIST to discover per-field keys is forbidden.
#[derive(Debug, Clone, PartialEq)]
pub struct FdeSet {
    /// Each field's clusters, and where its vectors start in `.fdev`.
    fields: Vec<(FdeClusters, u64)>,
}

impl FdeSet {
    /// Clusters each `(field, encoded rows)`; the second value is the `.fdev` blob, the fields'
    /// vectors back to back.
    #[must_use]
    pub fn build(fields: &[(String, Vec<Vec<f32>>)], params: Params) -> (Self, Vec<u8>) {
        let mut blob = Vec::new();
        let mut out = Vec::new();
        for (name, rows) in fields {
            let (c, v) = FdeClusters::build(name, rows, params);
            out.push((c, blob.len() as u64));
            blob.extend(v);
        }
        (Self { fields: out }, blob)
    }

    /// The field's clusters and where its vectors start, if the set has the field.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<(&FdeClusters, u64)> {
        self.fields
            .iter()
            .find(|(c, _)| c.field() == name)
            .map(|(c, at)| (c, *at))
    }

    /// Fields in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether the set holds no field.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Whether any field has more than one list (the point of writing the sidecar).
    #[must_use]
    pub fn worth_writing(&self) -> bool {
        self.fields.iter().any(|(c, _)| c.lists() > 1)
    }

    /// The `.fdec` bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(SET_MAGIC);
        b.extend((self.fields.len() as u32).to_le_bytes());
        for (c, at) in &self.fields {
            let e = c.encode();
            b.extend((e.len() as u32).to_le_bytes());
            b.extend(e);
            b.extend(at.to_le_bytes());
        }
        b
    }

    /// Reads a set back, or `None` for anything truncated, mislabelled, with a field twice, or
    /// with trailing bytes.
    #[must_use]
    pub fn decode(raw: &[u8]) -> Option<Self> {
        let mut at = 0usize;
        let mut take = |n: usize| -> Option<&[u8]> {
            let s = raw.get(at..at.checked_add(n)?)?;
            at += n;
            Some(s)
        };
        if take(4)? != SET_MAGIC {
            return None;
        }
        let n = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let mut fields: Vec<(FdeClusters, u64)> = Vec::new();
        for _ in 0..n {
            let len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            let c = FdeClusters::decode(take(len)?)?;
            let base = u64::from_le_bytes(take(8)?.try_into().ok()?);
            if fields.iter().any(|(o, _)| o.field() == c.field()) {
                return None;
            }
            fields.push((c, base));
        }
        (at == raw.len()).then_some(Self { fields })
    }

    /// Writes both objects.
    ///
    /// # Errors
    /// A store failure.
    pub async fn put<S: BlobStore>(
        &self,
        store: &S,
        segment_key: &Key,
        vectors: Vec<u8>,
    ) -> Result<(), pstore_blob::BlobError> {
        store.put(&vectors_key(segment_key), vectors.into()).await?;
        store.put(&key(segment_key), self.encode().into()).await?;
        Ok(())
    }

    /// Reads the set for `segment_key`.
    ///
    /// # Errors
    /// A store failure, or [`FormatError::Corrupt`] for a damaged sidecar.
    pub async fn open<S: BlobStore>(store: &S, segment_key: &Key) -> Result<Self, FormatError> {
        let raw = store
            .get(&key(segment_key))
            .await
            .map_err(|_| FormatError::Truncated)?;
        Self::decode(&raw).ok_or(FormatError::Corrupt("damaged multi-vector cluster sidecar"))
    }
}
