//! Opening and reading a segment.

use crate::codec::{Dec, checksum};
use crate::{BlockMeta, Document, FOOTER_LEN, Filter, FormatError, MAGIC, SUFFIX_FETCH, VERSION};
use bytes::Bytes;
use pstore_blob::{BlobStore, Key};
use std::collections::BTreeMap;

/// An opened segment: the index section, in memory, and nothing else.
#[derive(Debug, Clone)]
pub struct Segment {
    blocks: Vec<BlockMeta>,
    rows: u32,
}

impl Segment {
    /// Opens a segment knowing only its key.
    ///
    /// **At most two sequential reads.** One suffix range fetches the footer and, if the
    /// segment is small, its whole index section; only a segment whose index section did
    /// not fit needs the second.
    pub async fn open<S: BlobStore>(store: &S, key: &Key) -> Result<Self, FormatError> {
        // One suffix read. No `head` first: it is billed as a read, so requiring one
        // would double every cold open -- found by the request counter, not by inspection.
        let tail = store.get_suffix(key, SUFFIX_FETCH).await?;

        let foot_at = tail
            .len()
            .checked_sub(FOOTER_LEN)
            .ok_or(FormatError::Truncated)?;
        let foot = tail.get(foot_at..).ok_or(FormatError::Truncated)?;
        let mut d = Dec::new(foot);
        if d.raw(8)? != MAGIC {
            return Err(FormatError::Corrupt("bad magic"));
        }
        let version = d.u16()?;
        if version != VERSION {
            return Err(FormatError::UnsupportedVersion(version));
        }
        let index_offset = d.u64()?;
        let index_len = d.u32()?;
        let rows = d.u32()?;
        let sum = d.u64()?;
        // The trailing magic is what distinguishes a real footer from bytes that happen to
        // decode: a tampered length field almost never leaves both copies intact.
        if d.raw(8)? != MAGIC {
            return Err(FormatError::Corrupt("bad trailing magic"));
        }

        // Where in the object the bytes we hold begin. Derived from the footer's own
        // offsets rather than from a separately-fetched length.
        let seg_len = index_offset + u64::from(index_len) + FOOTER_LEN as u64;
        let tail_start = seg_len.saturating_sub(tail.len() as u64);
        let idx_bytes = {
            if index_offset >= tail_start {
                let lo = (index_offset - tail_start) as usize;
                let hi = lo
                    .checked_add(index_len as usize)
                    .ok_or(FormatError::Truncated)?;
                Bytes::copy_from_slice(tail.get(lo..hi).ok_or(FormatError::Truncated)?)
            } else {
                // Only now, and only for a segment whose index section is large.
                store
                    .get_range(key, index_offset..index_offset + u64::from(index_len))
                    .await?
            }
        };
        if checksum(&idx_bytes) != sum {
            return Err(FormatError::ChecksumMismatch);
        }

        Ok(Self {
            blocks: Self::decode_index(&idx_bytes)?,
            rows,
        })
    }

    fn decode_index(bytes: &[u8]) -> Result<Vec<BlockMeta>, FormatError> {
        let mut d = Dec::new(bytes);
        let n = d.u32()? as usize;
        let mut blocks = Vec::with_capacity(n.min(1 << 20));
        for _ in 0..n {
            let (offset, len, rows) = (d.u64()?, d.u32()?, d.u32()?);
            let zn = d.u32()? as usize;
            let mut zones = BTreeMap::new();
            for _ in 0..zn {
                let k = d.string()?;
                zones.insert(k, (d.i64()?, d.i64()?));
            }
            blocks.push(BlockMeta {
                offset,
                len,
                rows,
                zones,
            });
        }
        Ok(blocks)
    }

    /// Rows in the segment.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows as usize
    }

    /// Blocks in the segment.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Which blocks a filter cannot rule out.
    ///
    /// A block is kept unless its zone map proves no row in it can match. **Erring toward
    /// keeping is mandatory**: a wrongly dropped block silently loses rows.
    #[must_use]
    pub fn blocks_to_read(&self, filter: Option<&Filter>) -> Vec<usize> {
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| match filter {
                None => true,
                Some(f) => match b.zones.get(f.column()) {
                    Some((lo, hi)) => f.could_match(*lo, *hi),
                    // No zone map for that column -- it may be a string, or absent from
                    // this block. Cannot prune, so must read.
                    None => true,
                },
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Reads the rows a filter selects.
    ///
    /// The blocks that survive pruning are fetched in **one coalesced round**, so scanning
    /// costs one round trip regardless of how many blocks match.
    pub async fn scan<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        filter: Option<&Filter>,
    ) -> Result<Vec<Document>, FormatError> {
        let wanted = self.blocks_to_read(filter);
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let ranges: Vec<_> = wanted
            .iter()
            .filter_map(|i| self.blocks.get(*i))
            .map(|b| b.offset..b.offset + u64::from(b.len))
            .collect();
        let bufs = store.get_ranges(key, &ranges).await?;

        let mut out = Vec::new();
        for buf in &bufs {
            for doc in Self::decode_block(buf)? {
                if filter.is_none_or(|f| f.matches(&doc)) {
                    out.push(doc);
                }
            }
        }
        Ok(out)
    }

    /// Exact k-nearest-neighbour search by squared L2, over the rows a filter selects.
    ///
    /// Brute force, deliberately (D-10): below a few hundred thousand vectors it is
    /// faster than an approximate index, exactly accurate, needs no maintenance, and
    /// covers the long tail of a millions-of-indexes product. It is also the reference
    /// the approximate path will be measured against, so it must be *exactly* right.
    ///
    /// Squared distance, not the root: monotonic in the same order, and one fewer
    /// operation per vector on a path that is memory-bandwidth-bound anyway.
    pub async fn search<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
    ) -> Result<Vec<(String, f32)>, FormatError> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let docs = self.scan(store, key, filter).await?;
        let mut scored = Vec::with_capacity(docs.len());
        for d in docs {
            if d.vector.len() != query.len() {
                return Err(FormatError::DimensionMismatch {
                    expected: d.vector.len(),
                    got: query.len(),
                });
            }
            let dist: f32 = d
                .vector
                .iter()
                .zip(query)
                .map(|(a, b)| (a - b) * (a - b))
                .sum();
            scored.push((d.id, dist));
        }
        // `total_cmp` rather than `partial_cmp`: a NaN from a malformed vector must order
        // deterministically instead of making the sort's behaviour undefined. Ties break
        // on id so the result is reproducible, which is what lets a reference test exist.
        scored.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(k);
        Ok(scored)
    }

    /// ⚠️ Delegates rather than duplicating. This was a second decoder for the same wire
    /// format, byte-identical to `decode_docs` — which means every future change to the
    /// document encoding had to be made twice, and the day one of them was missed the
    /// reader and the writer would disagree with no compiler to say so.
    fn decode_block(buf: &[u8]) -> Result<Vec<Document>, FormatError> {
        crate::decode_docs(buf)
    }
}
