//! Opening and reading a segment.

use crate::codec::{Dec, checksum};
use crate::{
    BlockMeta, Document, FOOTER_LEN, Filter, FormatError, MAGIC, SUFFIX_FETCH, Section, VERSION,
};
use bytes::Bytes;
use pstore_blob::{BlobStore, Key};
use std::collections::BTreeMap;

/// An opened segment: the index section, in memory, and nothing else.
#[derive(Debug, Clone)]
pub struct Segment {
    blocks: Vec<BlockMeta>,
    rows: u32,
    /// Where each section lives, from the footer-addressed directory.
    sections: BTreeMap<u16, std::ops::Range<u64>>,
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
        let meta_offset = d.u64()?;
        let meta_len = d.u32()?;
        let rows = d.u32()?;
        let sum = d.u64()?;
        // The trailing magic is what distinguishes a real footer from bytes that happen to
        // decode: a tampered length field almost never leaves both copies intact.
        if d.raw(8)? != MAGIC {
            return Err(FormatError::Corrupt("bad trailing magic"));
        }

        // Where in the object the bytes we hold begin. Derived from the footer's own
        // offsets rather than from a separately-fetched length.
        let seg_len = meta_offset + u64::from(meta_len) + FOOTER_LEN as u64;
        let tail_start = seg_len.saturating_sub(tail.len() as u64);
        let idx_bytes = {
            if meta_offset >= tail_start {
                let lo = (meta_offset - tail_start) as usize;
                let hi = lo
                    .checked_add(meta_len as usize)
                    .ok_or(FormatError::Truncated)?;
                Bytes::copy_from_slice(tail.get(lo..hi).ok_or(FormatError::Truncated)?)
            } else {
                // Only now, and only for a segment whose meta region is large.
                store
                    .get_range(key, meta_offset..meta_offset + u64::from(meta_len))
                    .await?
            }
        };
        if checksum(&idx_bytes) != sum {
            return Err(FormatError::ChecksumMismatch);
        }

        // The meta region is the directory followed by the block index. Parsing the
        // directory tells us where the block index is; everything else in it is a byte
        // range a later fetch may or may not use.
        let mut d = Dec::new(&idx_bytes);
        let entries = d.u32()? as usize;
        let mut sections = BTreeMap::new();
        let mut blocks_span: Option<std::ops::Range<u64>> = None;
        for _ in 0..entries {
            let id = d.u16()?;
            let offset = d.u64()?;
            let len = d.u64()?;
            let span = offset..offset.saturating_add(len);
            // ⚠️ Recorded by RAW id, including ids this version has never heard of. There
            // is deliberately no "is this known?" check: `section()` is looked up by a
            // `Section`, so an unknown id is unreachable and storing it costs one map entry.
            // A check would be equivalent code that no test could distinguish — and
            // refusing would make every future section a breaking change for every deployed
            // reader at once, which segments being immutable makes permanent.
            if id == Section::Blocks as u16 {
                blocks_span = Some(span.clone());
            }
            sections.insert(id, span);
        }
        let blocks_span = blocks_span.ok_or(FormatError::Corrupt("no block section"))?;
        // The block index sits inside the region already fetched, at a known offset.
        let lo = (blocks_span.start - meta_offset) as usize;
        let hi = lo
            .checked_add((blocks_span.end - blocks_span.start) as usize)
            .ok_or(FormatError::Truncated)?;
        let blocks = idx_bytes.get(lo..hi).ok_or(FormatError::Truncated)?;

        Ok(Self {
            blocks: Self::decode_index(blocks)?,
            rows,
            sections,
        })
    }

    /// The byte offset just past the last data block.
    ///
    /// Data blocks are addressed by the index rather than by the directory, so nothing else
    /// can tell whether a section overlaps them.
    #[must_use]
    pub fn data_end(&self) -> u64 {
        self.blocks
            .iter()
            .map(|b| b.offset + u64::from(b.len))
            .max()
            .unwrap_or(0)
    }

    /// Where a section lives, or `None` if this segment does not carry it.
    #[must_use]
    pub fn section(&self, section: Section) -> Option<std::ops::Range<u64>> {
        self.sections.get(&(section as u16)).cloned()
    }

    /// Fetches a whole section.
    ///
    /// One ranged read. The caller decides *which* section, which is how a rung-0 scan
    /// avoids the full-precision bytes entirely rather than fetching and discarding them.
    pub async fn fetch_section<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        section: Section,
    ) -> Result<Option<Bytes>, FormatError> {
        let Some(span) = self.section(section) else {
            return Ok(None);
        };
        Ok(Some(store.get_range(key, span).await?))
    }

    /// Bytes per row in the vectors section, or 0 if there is none.
    fn vector_row_len(&self) -> usize {
        match (self.section(Section::Vectors), self.rows) {
            (Some(span), rows) if rows > 0 => (span.end - span.start) as usize / rows as usize,
            _ => 0,
        }
    }

    /// The full-precision vectors for **specific rows**.
    ///
    /// ⚠️ The rerank path's fetch, and the reason it is not [`Self::vectors`]: rung 2 scores
    /// a few hundred survivors, and reading the whole section to reach them costs the
    /// segment's entire bandwidth for a query that looks at a fraction of a percent of it.
    /// Rows are fixed-width, so each is a computable range and the coalescer decides which
    /// of the holes between them are cheaper to fetch than to skip.
    pub async fn vector_rows<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        rows: &[usize],
    ) -> Result<BTreeMap<usize, Vec<f32>>, FormatError> {
        let per_row = self.vector_row_len();
        let Some(span) = self.section(Section::Vectors) else {
            return Ok(BTreeMap::new());
        };
        if per_row == 0 || rows.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut wanted: Vec<usize> = rows.to_vec();
        wanted.sort_unstable();
        wanted.dedup();
        let ranges: Vec<std::ops::Range<u64>> = wanted
            .iter()
            .map(|r| {
                let lo = span.start + (*r * per_row) as u64;
                lo..lo + per_row as u64
            })
            .collect();
        let bufs = store.get_ranges(key, &ranges).await?;
        Ok(wanted
            .into_iter()
            .zip(bufs)
            .map(|(row, raw)| {
                let v = raw
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes(b.try_into().unwrap_or([0; 4])))
                    .collect();
                (row, v)
            })
            .collect())
    }

    /// The full-precision vectors, one row at a time.
    ///
    /// Width is derived by dividing the section by the row count, so the format needs no
    /// separate dimension field and cannot disagree with itself about one.
    pub async fn vectors<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
    ) -> Result<Vec<Vec<f32>>, FormatError> {
        let Some(raw) = self.fetch_section(store, key, Section::Vectors).await? else {
            return Ok(Vec::new());
        };
        if self.rows == 0 {
            return Ok(Vec::new());
        }
        let per_row = raw.len() / self.rows as usize;
        Ok(raw
            .chunks_exact(per_row.max(1))
            .map(|row| {
                row.chunks_exact(4)
                    .map(|b| f32::from_le_bytes(b.try_into().unwrap_or([0; 4])))
                    .collect()
            })
            .collect())
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
        // Row indices, so a decoded row can find its own vector. Blocks are contiguous and
        // in order, so the base is the sum of the rows before this block.
        let mut base: Vec<usize> = Vec::with_capacity(self.blocks.len());
        let mut running = 0usize;
        for b in &self.blocks {
            base.push(running);
            running += b.rows as usize;
        }

        // ⚠️ Only the vector rows belonging to the blocks being read, not the whole
        // section. Fetching all of it would undo zone-map pruning in bytes while leaving it
        // intact in block count -- the plan would still say "one block" and the wire would
        // still carry the entire segment's vectors.
        let per_row = self.vector_row_len();
        let mut all: Vec<std::ops::Range<u64>> = ranges;
        let block_count = all.len();
        if let (Some(vec_span), true) = (self.section(Section::Vectors), per_row > 0) {
            for i in &wanted {
                let (Some(start), Some(b)) = (base.get(*i), self.blocks.get(*i)) else {
                    continue;
                };
                let lo = vec_span.start + (*start * per_row) as u64;
                all.push(lo..lo + (b.rows as usize * per_row) as u64);
            }
        }
        // ⚠️ One call, so blocks and vector rows go out TOGETHER. They are different
        // sections of the same object, both spans are known before either is issued, and
        // awaiting one before the other would make every scan a two-hop read for no reason
        // a caller could see. Width is free; depth is not.
        let bufs = store.get_ranges(key, &all).await?;

        let mut out = Vec::new();
        for (n, _) in wanted.iter().enumerate() {
            let Some(buf) = bufs.get(n) else { continue };
            let rows = Self::decode_block(buf)?;
            let vecs = bufs.get(block_count + n);
            for (r, mut doc) in rows.into_iter().enumerate() {
                if let Some(raw) = vecs.and_then(|v| v.get(r * per_row..(r + 1) * per_row)) {
                    doc.vector = raw
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes(b.try_into().unwrap_or([0; 4])))
                        .collect();
                }
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

    /// Decodes one block's rows. Vectors arrive separately, from their own section.
    fn decode_block(buf: &[u8]) -> Result<Vec<Document>, FormatError> {
        crate::decode_rows(buf)
    }
}
