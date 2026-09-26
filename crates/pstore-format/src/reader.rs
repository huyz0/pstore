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
    /// The vector fields this segment carries.
    fields: Vec<crate::FieldLayout>,
    text_fields: Vec<String>,
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
        // ⚠️ `Meta`, not the default `Bulk`. This suffix IS the index section for most
        // segments, it is read by every query on that segment, and it is <0.1% of the bytes
        // — D-21 exists so a burst of scan traffic cannot evict it.
        let tail = store
            .get_suffix_as(key, SUFFIX_FETCH, pstore_blob::Class::Meta)
            .await?;

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
                // Only now, and only for a segment whose meta region is large. ⚠️ Still
                // `Meta`: it is the same index section, just too big to have arrived with
                // the footer, and it is exactly as valuable.
                store
                    .get_range_as(
                        key,
                        meta_offset..meta_offset + u64::from(meta_len),
                        pstore_blob::Class::Meta,
                    )
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

        let mut seg = Self {
            blocks: Self::decode_index(blocks)?,
            rows,
            sections,
            fields: Vec::new(),
            text_fields: Vec::new(),
        };
        seg.fields = seg.decode_fields(&idx_bytes, meta_offset)?;
        seg.text_fields = seg.decode_text_fields(&idx_bytes, meta_offset)?;
        Ok(seg)
    }

    /// Parses the `Fields` table, or synthesises the legacy one-dense-field view.
    ///
    /// ⚠️ A segment without the table is not an error and not empty: it is exactly today's
    /// segment, one dense field named [`crate::DEFAULT_FIELD`]. That is the whole point of
    /// putting the table in its own section — `modalities-and-sequencing.md` §3 promises old
    /// segments stay valid forever.
    fn decode_fields(
        &self,
        meta: &[u8],
        meta_offset: u64,
    ) -> Result<Vec<crate::FieldLayout>, FormatError> {
        let Some(span) = self.section(Section::Fields) else {
            return Ok(if self.section(Section::Vectors).is_some() {
                vec![crate::FieldLayout {
                    name: crate::DEFAULT_FIELD.to_owned(),
                    kind: 0,
                    metric: 0,
                    dims: self.vector_row_len() as u32 / 4,
                    per_row: 1,
                    vectors: Section::Vectors as u16,
                    rabitq: Section::RaBitQ as u16,
                    sq8: Section::Sq8 as u16,
                }]
            } else {
                Vec::new()
            });
        };
        // The table lives outside the meta region, so it needs its own read unless it
        // happens to fall inside the bytes already fetched.
        let lo = span.start.checked_sub(meta_offset).unwrap_or(u64::MAX) as usize;
        let raw = meta
            .get(lo..lo + (span.end - span.start) as usize)
            .ok_or(FormatError::Truncated)?;
        let mut d = Dec::new(raw);
        let n = d.u32()? as usize;
        let mut out = Vec::with_capacity(n.min(1 << 12));
        for _ in 0..n {
            out.push(crate::FieldLayout {
                name: d.string()?,
                kind: d.u8()?,
                metric: d.u8()?,
                dims: d.u32()?,
                per_row: d.u32()?,
                vectors: d.u16()?,
                rabitq: d.u16()?,
                sq8: d.u16()?,
            });
        }
        Ok(out)
    }

    /// Parses the `TextFields` table, or reads absence as the pre-M6c default.
    ///
    /// ⚠️ **Absence is not "no text field".** Every segment written before this section
    /// existed was built over [`crate::text::DEFAULT_TEXT_FIELD`], and reading absence as
    /// empty would refuse every text query against every one of them — which is all of them.
    /// A segment carrying no postings at all is the other case, and that one *is* empty.
    fn decode_text_fields(
        &self,
        meta: &[u8],
        meta_offset: u64,
    ) -> Result<Vec<String>, FormatError> {
        let Some(span) = self.section(Section::TextFields) else {
            return Ok(if self.section(Section::TextPostings).is_some() {
                vec![crate::text::DEFAULT_TEXT_FIELD.to_owned()]
            } else {
                Vec::new()
            });
        };
        let lo = span.start.checked_sub(meta_offset).unwrap_or(u64::MAX) as usize;
        let raw = meta
            .get(lo..lo + (span.end - span.start) as usize)
            .ok_or(FormatError::Truncated)?;
        let mut d = Dec::new(raw);
        let n = d.u32()? as usize;
        let mut out = Vec::with_capacity(n.min(1 << 12));
        for _ in 0..n {
            out.push(d.string()?);
        }
        Ok(out)
    }

    /// Which attribute(s) this segment's text index was built over.
    ///
    /// ⚠️ **Ask this rather than assuming `"text"`.** A reader that compares a requested field
    /// against the constant is right only while every segment is built over the constant, and
    /// once one is not, it answers the wrong field's ranking with nothing saying so — D-73 at
    /// field granularity.
    #[must_use]
    pub fn text_fields(&self) -> &[String] {
        &self.text_fields
    }

    /// Every vector field this segment carries.
    #[must_use]
    pub fn fields(&self) -> &[crate::FieldLayout] {
        &self.fields
    }

    /// The layout of one named field.
    #[must_use]
    pub fn field_layout(&self, name: &str) -> Option<&crate::FieldLayout> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Whether this segment carries a full-text index.
    ///
    /// ⚠️ Answered by the **section**, not by a `Fields` row, and deliberately: that table
    /// describes *vector* fields, whose layout a reader must know to decode them. A row for a
    /// text field would send its postings to `decode_field`, which reads them as `f32` and
    /// returns a dense field of noise — the failure a sparse field's `kind` guard exists for,
    /// reintroduced by a table entry nothing needed.
    #[must_use]
    pub fn has_text(&self) -> bool {
        self.section(Section::TextPostings).is_some()
    }

    /// The segment's sparse field, if it has one.
    ///
    /// ⚠️ At most one in M5a: a second would need its own section id pair, the way
    /// `FieldVectors` mirrors `Vectors`, and one sparse field is what fusion needs. The
    /// **first** in name order is returned rather than an arbitrary one, so a segment that
    /// somehow carries two behaves deterministically instead of differently per read.
    #[must_use]
    pub fn sparse_field(&self) -> Option<&str> {
        self.fields
            .iter()
            .find(|f| f.kind == 1)
            .map(|f| f.name.as_str())
    }

    /// Where one field's `kind` section lives.
    #[must_use]
    pub fn field_section(&self, name: &str, kind: Section) -> Option<std::ops::Range<u64>> {
        let f = self.field_layout(name)?;
        let id = match kind {
            Section::Vectors | Section::FieldVectors => f.vectors,
            Section::RaBitQ | Section::FieldRaBitQ => f.rabitq,
            Section::Sq8 | Section::FieldSq8 => f.sq8,
            other => other as u16,
        };
        self.sections.get(&id).cloned()
    }

    /// Several fields' vectors, fetched **together**.
    ///
    /// ⚠️ One round trip, whatever the field count. Every span is known once the segment is
    /// open, so a hybrid query reading a dense and a sparse field pays bytes rather than a
    /// hop per modality. Reading them in a loop would be functionally identical and turn
    /// `prefetch[]` into a depth multiplier.
    pub async fn read_fields<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        names: &[&str],
    ) -> Result<BTreeMap<String, Vec<Vec<Vec<f32>>>>, FormatError> {
        let mut layouts = Vec::with_capacity(names.len());
        for n in names {
            layouts.push(
                self.field_layout(n)
                    .ok_or(FormatError::UnknownField)?
                    .clone(),
            );
        }
        let ranges: Vec<std::ops::Range<u64>> = names
            .iter()
            .filter_map(|n| self.field_section(n, Section::Vectors))
            .collect();
        let bufs = store.get_ranges(key, &ranges).await?;
        let mut out = BTreeMap::new();
        for ((name, layout), raw) in names.iter().zip(&layouts).zip(bufs) {
            out.insert(
                (*name).to_owned(),
                decode_field(&raw, layout, self.rows as usize)?,
            );
        }
        Ok(out)
    }

    /// One field's vectors, one entry per row.
    ///
    /// An absent field is an **error**: a miss returning zero rows is indistinguishable
    /// from a legitimately empty field, so a caller could not tell a typo from data.
    pub async fn field_vectors<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        name: &str,
    ) -> Result<Vec<Vec<Vec<f32>>>, FormatError> {
        let f = self
            .field_layout(name)
            .ok_or(FormatError::UnknownField)?
            .clone();
        // ⚠️ A sparse field's layout row points at `SparsePostings`, so asking for its
        // "vectors" here would fetch postings and decode them as f32 — garbage, silently, in
        // the shape of a dense field. Sparse is reconstructed by `scan`, which has the
        // dictionary; there is nothing dense to return.
        if f.kind == 1 {
            return Ok(vec![Vec::new(); self.rows as usize]);
        }
        let Some(span) = self.field_section(name, Section::Vectors) else {
            return Ok(vec![Vec::new(); self.rows as usize]);
        };
        let raw = store.get_range(key, span).await?;
        decode_field(&raw, &f, self.rows as usize)
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
    /// ⚠️ **`row_count`, not `index_row_count`.** `Vectors` is full precision — 1,536 bytes a
    /// row at 384d against 64 for the 1-bit code — so it is the one section replication must
    /// **not** duplicate. The codes stride by the index row count and this does not, which is
    /// the whole reason the two counts are separate rather than one renaming of the other.
    fn vector_row_len(&self) -> usize {
        match (self.section(Section::Vectors), self.rows) {
            (Some(span), rows) if rows > 0 => (span.end - span.start) as usize / rows as usize,
            _ => 0,
        }
    }

    /// How many rows the **1-bit and int8 code** sections cover, which is ≥ [`Self::row_count`].
    ///
    /// ⚠️ **Derived from the section's length, so it costs no request.** A boundary vector's
    /// codes appear once per posting list it belongs to while its document appears once, so
    /// `RaBitQ` and `Sq8` stride by this while the blocks **and `Vectors`** stride by
    /// `row_count`. Taking the wrong one reads every row after the first at an offset and
    /// decodes without complaint.
    ///
    /// ⚠️ `Vectors` is deliberately on the other side of that line: at 1,536 bytes a row
    /// against 64 for the code, duplicating it would spend the storage this design exists to
    /// avoid.
    #[must_use]
    pub fn index_row_count(&self) -> usize {
        self.section(Section::IndexRows)
            .map_or(self.rows as usize, |s| (s.end - s.start) as usize / 4)
    }

    /// Which data row each index row names.
    ///
    /// ⚠️ In the **body**, not the meta region, so this is a fetch — the mapping is 4 bytes a
    /// row and only a probe's rows are ever needed, which is why `index_row_count` is derived
    /// from the directory instead.
    ///
    /// # Errors
    /// If the store refuses or the section is truncated.
    pub async fn index_rows<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
    ) -> Result<Vec<u32>, FormatError> {
        let Some(span) = self.section(Section::IndexRows) else {
            // Absent means the two spaces are the same one.
            return Ok((0..self.rows).collect());
        };
        let raw = store
            .get_range_as(key, span, pstore_blob::Class::Meta)
            .await?;
        Ok(raw
            .chunks_exact(4)
            .filter_map(|c| c.try_into().ok().map(u32::from_le_bytes))
            .collect())
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
        // Two returns, not one `||`: a row width of 0 must never reach `get_ranges`, whose
        // empty ranges are neither fetched nor RETURNED, so a store that pairs results with
        // requests by position would misplace them. No rows is simply nothing to fetch.
        if per_row == 0 {
            return Ok(BTreeMap::new());
        }
        if rows.is_empty() {
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
        // ⚠️ The postings ride in the SAME call, and the dictionary sidecar is joined with
        // it rather than awaited after. Both are addressed without reading anything first —
        // the span comes from the field's layout row, the key by derivation — so fetching
        // them in sequence would turn every scan of a sparse segment into three rounds for
        // no reason a caller could see.
        let sparse_name = self.sparse_field().map(str::to_owned);
        let postings_at = sparse_name.as_ref().and_then(|n| {
            let span = self.field_section(n, Section::SparsePostings)?;
            all.push(span);
            Some(all.len() - 1)
        });
        // ⚠️ One call, so blocks and vector rows go out TOGETHER. They are different
        // sections of the same object, both spans are known before either is issued, and
        // awaiting one before the other would make every scan a two-hop read for no reason
        // a caller could see. Width is free; depth is not.
        let (bufs, dict_raw) = if postings_at.is_some() {
            let (b, d) = futures_util::future::join(
                store.get_ranges(key, &all),
                store.get_immutable(&crate::sparse::dict_key(key), pstore_blob::Class::Pinned),
            )
            .await;
            (b?, d.ok())
        } else {
            (store.get_ranges(key, &all).await?, None)
        };
        // Row -> its `(dimension, impact)` pairs. Empty unless this segment carries a sparse
        // field AND its dictionary was reachable; a missing sidecar is a read that cannot
        // reconstruct, and returning the field as *absent* would be the silent loss this
        // whole path exists to prevent.
        let sparse_rows: Vec<Vec<(u32, crate::Impact)>> = match (&postings_at, &dict_raw) {
            (Some(at), Some(raw)) => {
                let dict = crate::sparse::Dictionary::decode(raw)
                    .ok_or(FormatError::Corrupt("sparse dictionary"))?;
                let section = bufs.get(*at).ok_or(FormatError::Truncated)?;
                crate::sparse::transpose(&dict, section, self.rows as usize)
            }
            (Some(_), None) => {
                return Err(FormatError::Corrupt(
                    "a segment carries a sparse field but its dictionary sidecar is missing",
                ));
            }
            _ => Vec::new(),
        };

        // ⚠️ Every field, not just the first. `scan` returns whole documents, and a
        // document that comes back missing a field it was written with is silent loss at
        // the read side — the mirror of the write-side bug this milestone opened and closed.
        // ⚠️ The inline path below reads one fixed-width vector per row and prunes to the
        // blocks the filter selected — the common case, and the one where zone-map pruning
        // saves bytes. It cannot serve a variable-width field (several vectors per row), so
        // anything that is not exactly one fixed-width field is read whole, per field.
        let simple = self.fields.len() == 1 && self.fields.first().is_some_and(|f| f.per_row == 1);
        let extra = if simple {
            Vec::new()
        } else {
            let mut m: Vec<(String, Vec<Vec<Vec<f32>>>)> = Vec::new();
            for f in &self.fields {
                if f.kind == 1 {
                    continue;
                }
                m.push((
                    f.name.clone(),
                    self.field_vectors(store, key, &f.name).await?,
                ));
            }
            m
        };
        let first_field = self
            .fields
            .first()
            .map_or_else(|| crate::DEFAULT_FIELD.to_owned(), |f| f.name.clone());

        let mut out = Vec::new();
        for (n, block) in wanted.iter().enumerate() {
            let Some(buf) = bufs.get(n) else { continue };
            let rows = Self::decode_block(buf)?;
            let vecs = bufs.get(block_count + n);
            // The absolute row index this block starts at, so a per-row lookup into a
            // whole-field vector lands on the right row.
            let start = base.get(*block).copied().unwrap_or(0);
            for (r, mut doc) in rows.into_iter().enumerate() {
                if let Some(raw) = vecs
                    .filter(|_| simple)
                    .and_then(|v| v.get(r * per_row..(r + 1) * per_row))
                {
                    let v: Vec<f32> = raw
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes(b.try_into().unwrap_or([0; 4])))
                        .collect();
                    doc.vectors
                        .insert(first_field.clone(), crate::VectorField::dense(v));
                }
                for (name, all) in &extra {
                    if let Some(v) = all.get(start + r) {
                        doc.vectors
                            .insert(name.clone(), crate::VectorField::Dense(v.clone()));
                    }
                }
                if let Some(name) = &sparse_name
                    && let Some(pairs) = sparse_rows.get(start + r)
                {
                    // ⚠️ Overwrites whatever the `extra` loop left, which for a sparse field
                    // is an empty dense vector: a field present-but-empty is not the same
                    // document as the one that was written.
                    doc.vectors
                        .insert(name.clone(), crate::VectorField::Sparse(pairs.clone()));
                }
                if filter.is_none_or(|f| f.matches(&doc)) {
                    out.push(doc);
                }
            }
        }
        Ok(out)
    }

    /// The ids of specific rows, reading **only the blocks that hold them**.
    ///
    /// ⚠️ **This exists so that resolving a search hit does not read a segment.** A query
    /// answers with `(segment, row)` pairs, and the only other way to turn a row ordinal into
    /// an id is `scan`, which fetches every block the filter does not prune plus every
    /// vector — a read that scales with **documents**, which is one of `AGENTS.md`'s Nevers.
    /// Here the block holding a row is arithmetic on metadata already in hand, the blocks are
    /// fetched in one coalesced round however many there are, and no vector section is
    /// touched at all: an id lives in the block payload.
    ///
    /// Returns one entry per requested row, in request order. A row the segment does not
    /// have is `None` — never another row's id, which would be a wrong search result with
    /// nothing to notice it.
    ///
    /// # Errors
    /// If a block cannot be read or decoded.
    pub async fn ids_at<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        rows: &[usize],
    ) -> Result<Vec<Option<String>>, FormatError> {
        Ok(self
            .rows_at(store, key, rows)
            .await?
            .into_iter()
            .map(|r| r.map(|d| d.id))
            .collect())
    }

    /// Every row of the blocks `keep` does not rule out, with its absolute row number — the
    /// input to a query's filter mask (M9b).
    ///
    /// `keep` sees each block's zone map (integer attribute → `(min, max)`, empty for a
    /// zone-free segment) and must answer `true` unless no row in the block can match. The
    /// kept blocks are fetched in **one** coalesced `get_ranges`; no vector is read.
    ///
    /// # Errors
    /// If a block cannot be read or decoded.
    pub async fn rows_where<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        keep: impl Fn(&BTreeMap<String, (i64, i64)>) -> bool,
    ) -> Result<Vec<(usize, Document)>, FormatError> {
        let mut base = 0usize;
        let mut wanted: Vec<(usize, &BlockMeta)> = Vec::new();
        for b in &self.blocks {
            if keep(&b.zones) {
                wanted.push((base, b));
            }
            base += b.rows as usize;
        }
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let ranges: Vec<std::ops::Range<u64>> = wanted
            .iter()
            .map(|(_, b)| b.offset..b.offset + u64::from(b.len))
            .collect();
        let bufs = store.get_ranges(key, &ranges).await?;
        let mut out = Vec::new();
        for ((start, _), buf) in wanted.iter().zip(&bufs) {
            for (r, doc) in Self::decode_block(buf)?.into_iter().enumerate() {
                out.push((start + r, doc));
            }
        }
        Ok(out)
    }

    /// Specific rows' ids **and attributes**, from the same blocks [`Self::ids_at`] reads.
    ///
    /// ⚠️ **The same fetch, not a second one (M9a).** A block is the unit of both: it carries
    /// every row's id and attributes, and the id could only ever be had by fetching and
    /// decoding the whole block. Keeping the attributes that decode already produced is what
    /// makes `include_attributes` cost no request and no byte. Vectors are not in a block and
    /// are not returned: each [`Document`] here has an empty `vectors`.
    ///
    /// # Errors
    /// If a block cannot be read or decoded.
    pub async fn rows_at<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        rows: &[usize],
    ) -> Result<Vec<Option<Document>>, FormatError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        // The absolute row each block starts at. Blocks are contiguous and in order.
        let mut base = Vec::with_capacity(self.blocks.len());
        let mut running = 0usize;
        for b in &self.blocks {
            base.push(running);
            running += b.rows as usize;
        }
        let block_of = |row: usize| -> Option<usize> {
            self.blocks.iter().enumerate().position(|(i, b)| {
                let start = base.get(i).copied().unwrap_or(0);
                row >= start && row < start + b.rows as usize
            })
        };

        let mut wanted: Vec<usize> = rows.iter().filter_map(|r| block_of(*r)).collect();
        wanted.sort_unstable();
        wanted.dedup();
        if wanted.is_empty() {
            return Ok(rows.iter().map(|_| None).collect());
        }
        let ranges: Vec<std::ops::Range<u64>> = wanted
            .iter()
            .filter_map(|i| self.blocks.get(*i))
            .map(|b| b.offset..b.offset + u64::from(b.len))
            .collect();
        let bufs = store.get_ranges(key, &ranges).await?;

        // Row -> its document, for the rows the fetched blocks happen to carry.
        let mut found: BTreeMap<usize, Document> = BTreeMap::new();
        for (n, block) in wanted.iter().enumerate() {
            let Some(buf) = bufs.get(n) else { continue };
            let start = base.get(*block).copied().unwrap_or(0);
            for (r, doc) in Self::decode_block(buf)?.into_iter().enumerate() {
                found.insert(start + r, doc);
            }
        }
        Ok(rows.iter().map(|r| found.get(r).cloned()).collect())
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
            if d.vector().len() != query.len() {
                return Err(FormatError::DimensionMismatch {
                    expected: d.vector().len(),
                    got: query.len(),
                });
            }
            let dist: f32 = d
                .vector()
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

/// Decodes one field's vector section.
fn decode_field(
    raw: &[u8],
    f: &crate::FieldLayout,
    rows: usize,
) -> Result<Vec<Vec<Vec<f32>>>, FormatError> {
    let dims = f.dims as usize;
    let width = dims * 4;
    if dims == 0 {
        return Ok(vec![Vec::new(); rows]);
    }
    let read = |b: &[u8]| -> Vec<f32> {
        b.chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap_or([0; 4])))
            .collect()
    };
    if f.per_row == 0 {
        // Variable width: `rows + 1` offsets, so row `i` is `offsets[i]..offsets[i + 1]`
        // and the last row is not a special case.
        let table = (rows + 1) * 8;
        let offsets: Vec<u64> = raw
            .get(..table)
            .ok_or(FormatError::Truncated)?
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap_or([0; 8])))
            .collect();
        let mut out = Vec::with_capacity(rows);
        for i in 0..rows {
            let (lo, hi) = (
                *offsets.get(i).ok_or(FormatError::Truncated)? as usize,
                *offsets.get(i + 1).ok_or(FormatError::Truncated)? as usize,
            );
            // ⚠️ Checked. A corrupt offset table — 0xFF bytes, say — makes these add past
            // `usize::MAX` and panic on malformed input, which is the one thing a decoder
            // must never do. Found by writing the truncation test, not by reading this.
            let body = table
                .checked_add(lo)
                .zip(table.checked_add(hi))
                .and_then(|(a, b)| raw.get(a..b))
                .ok_or(FormatError::Truncated)?;
            out.push(body.chunks_exact(width).map(read).collect());
        }
        return Ok(out);
    }
    Ok(raw
        .chunks_exact(width)
        .map(|b| vec![read(b)])
        .take(rows)
        .collect())
}
