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
        let q = pstore_format::fde::encode_query(&pstore_format::fde::DEFAULT, query);
        let dim = self.centroids.first().map_or(0, Vec::len);
        let width = (dim * 4) as u64;
        let mut order: Vec<(usize, f32)> = self
            .centroids
            .iter()
            .enumerate()
            .map(|(i, c)| (i, dot(&q, c)))
            .collect();
        order.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        // Where each list starts, in rows of `.fdev`.
        let mut starts = Vec::with_capacity(self.lists.len());
        let mut at = 0u64;
        for l in &self.lists {
            starts.push(at);
            at += l.len() as u64;
        }
        let chosen: Vec<usize> = order.iter().take(probes.max(1)).map(|(i, _)| *i).collect();
        let ranges: Vec<std::ops::Range<u64>> = chosen
            .iter()
            .filter_map(|i| {
                let lo = *starts.get(*i)? * width;
                Some(lo..lo + self.lists.get(*i)?.len() as u64 * width)
            })
            .collect();
        let bufs = store.get_ranges(&vectors_key(segment_key), &ranges).await?;
        let mut best: std::collections::BTreeMap<usize, f32> = std::collections::BTreeMap::new();
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
                    .zip(&q)
                    .map(|(b, x)| f32::from_le_bytes(b.try_into().unwrap_or([0; 4])) * x)
                    .sum();
                best.entry(*row as usize).or_insert(score);
            }
        }
        let mut scored: Vec<(usize, f32)> = best.into_iter().collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.truncate(limit);
        Ok(scored)
    }
}
