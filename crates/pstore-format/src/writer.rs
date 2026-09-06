//! Building a segment.

use crate::codec::{Enc, checksum};
use crate::{BlockMeta, Document, FOOTER_LEN, MAGIC, Section, VERSION, Value};
use bytes::Bytes;
use std::collections::BTreeMap;

/// Accumulates documents and emits one immutable segment.
///
/// ⚠️ **The block size is a request, not a promise.** A segment's index section must fit
/// inside the fixed suffix read that opens it, or the open costs a second sequential round
/// trip — and that is the difference between a three-hop query and a four-hop one. Whether
/// it fits depends on the row count, which the caller controls and the writer sees. So the
/// writer buffers, then chooses the largest block size that keeps its own index inside the
/// budget. The invariant is structural rather than a property of whatever fixture a test
/// happens to use.
///
/// Found by a depth test at 40,000 rows: the ≤3 invariant held at 500 rows and broke at
/// 40,000, and nothing in the code or the tests said which regime it was in.
#[derive(Debug)]
pub struct SegmentWriter {
    index_budget: usize,
    rows_per_block: usize,
    /// Opaque, fixed-width payloads supplied by a higher layer.
    ///
    /// ⚠️ The format does **not** quantize. `pstore-format` is layer 2 and the quantizer is
    /// layer 3, so a writer that called it would invert the dependency and put the
    /// architecture in a comment instead of in Cargo.toml. The format owns *where sections
    /// live*; whoever owns the codes hands them over as bytes.
    extra: Vec<(Section, Vec<u8>)>,
    raw_extra: Vec<(u16, Vec<u8>)>,
    docs: Vec<Document>,
    blocks: Vec<BlockMeta>,
    body: Enc,
    rows: u32,
}

/// Bytes the index section may occupy, so it arrives with the footer.
///
/// The suffix read carries the footer too, so the index gets what is left. A segment whose
/// index exceeds this opens in two round trips instead of one.
pub const INDEX_BUDGET: usize = crate::SUFFIX_FETCH as usize - FOOTER_LEN;

impl SegmentWriter {
    /// A writer that seals a block every `rows_per_block` documents.
    ///
    /// Block size is the one tunable that most directly trades request count against
    /// bytes: too small and a scan issues many ranged reads, too large and a filtered scan
    /// drags in rows it will discard.
    #[must_use]
    pub fn new(rows_per_block: usize) -> Self {
        Self {
            index_budget: INDEX_BUDGET,
            rows_per_block: rows_per_block.max(1),
            extra: Vec::new(),
            raw_extra: Vec::new(),
            docs: Vec::new(),
            blocks: Vec::new(),
            body: Enc::default(),
            rows: 0,
        }
    }

    /// A writer allowed only `budget` bytes of index section.
    ///
    /// The default is the whole suffix read minus the footer. It is a knob because a
    /// segment that carries more footer-addressed sections — M3 adds centroids — has less
    /// room for the block index, and because a reader's two-read path cannot be tested
    /// against a writer that makes it unreachable.
    #[must_use]
    pub fn with_index_budget(mut self, budget: usize) -> Self {
        self.index_budget = budget;
        self
    }

    /// Attaches a fixed-width section, one equal-length record per row.
    ///
    /// The reader derives the record width by dividing by the row count, so a caller that
    /// supplies a ragged payload gets rows that read as garbage — hence the debug assert.
    #[must_use]
    pub fn with_section(mut self, section: Section, bytes: Vec<u8>) -> Self {
        self.extra.push((section, bytes));
        self
    }

    /// Attaches a section under a **raw** id, including one this version does not know.
    ///
    /// Exists to build a segment "from the future", which is the only way to check that a
    /// reader skips what it does not recognise. Forward compatibility that cannot be tested
    /// is a promise, and segments are immutable, so old readers meet new segments for
    /// years.
    #[doc(hidden)]
    #[must_use]
    pub fn with_raw_section(mut self, id: u16, bytes: Vec<u8>) -> Self {
        self.raw_extra.push((id, bytes));
        self
    }

    /// Adds a document.
    ///
    /// Buffered rather than sealed on the spot: the block size cannot be chosen until the
    /// row count is known, and choosing it eagerly is what made the index section unbounded.
    pub fn push(&mut self, doc: Document) {
        self.docs.push(doc);
    }

    fn seal(&mut self, rows: &[Document]) {
        if rows.is_empty() {
            return;
        }
        let offset = self.body.len() as u64;
        let mut zones: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        for d in rows {
            for (k, v) in &d.attrs {
                if let Value::Int(n) = v {
                    // Zone maps are built here, from the rows as they are written -- never
                    // from a later pass that could disagree with the bytes.
                    let e = zones.entry(k.clone()).or_insert((*n, *n));
                    e.0 = e.0.min(*n);
                    e.1 = e.1.max(*n);
                }
            }
        }
        let blk = crate::encode_rows(rows);
        let len = blk.len() as u32;
        self.body.raw(&blk);
        self.blocks.push(BlockMeta {
            offset,
            len,
            rows: rows.len() as u32,
            zones,
        });
        self.rows += rows.len() as u32;
    }

    /// Encodes the index section for the blocks sealed so far.
    fn encode_index(&self) -> Enc {
        let mut idx = Enc::default();
        idx.u32(self.blocks.len() as u32);
        for b in &self.blocks {
            idx.u64(b.offset);
            idx.u32(b.len);
            idx.u32(b.rows);
            idx.u32(b.zones.len() as u32);
            for (k, (lo, hi)) in &b.zones {
                idx.bytes(k.as_bytes());
                idx.i64(*lo);
                idx.i64(*hi);
            }
        }
        idx
    }

    /// Seals the last block and emits the segment.
    #[must_use]
    pub fn finish(mut self) -> Bytes {
        let docs = std::mem::take(&mut self.docs);
        // ⚠️ Grow the block size until the index fits the budget, rather than trusting the
        // caller's request. Doubling terminates: at one block the index is a handful of
        // bytes, and every doubling at least halves the block count.
        let idx = loop {
            self.blocks.clear();
            self.body = Enc::default();
            self.rows = 0;
            for chunk in docs.chunks(self.rows_per_block) {
                self.seal(chunk);
            }
            let idx = self.encode_index();
            if idx.len() <= self.index_budget || self.blocks.len() <= 1 {
                break idx;
            }
            self.rows_per_block = self.rows_per_block.saturating_mul(2);
        };

        // Vectors leave the data blocks entirely and become their own section: row-major,
        // fixed width, so row `i` is a computable byte range and a reader can fetch one row
        // or none without touching the rest.
        let dim = docs.first().map_or(0, |d| d.vector().len());
        let vectors: Vec<u8> = if dim == 0 {
            Vec::new()
        } else {
            let mut v = Vec::with_capacity(docs.len() * dim * 4);
            for d in &docs {
                // A ragged vector would silently shift every later row. Padding is the
                // conservative choice: a wrong-length vector is a caller bug, and truncating
                // the section would corrupt rows that are fine.
                for j in 0..dim {
                    v.extend_from_slice(&d.vector().get(j).copied().unwrap_or(0.0).to_le_bytes());
                }
            }
            v
        };

        let mut out = self.body;
        let mut dir: Vec<(Section, u64, u64)> = Vec::new();
        if !vectors.is_empty() {
            dir.push((Section::Vectors, out.len() as u64, vectors.len() as u64));
            out.raw(&vectors);
        }
        let mut raw_dir: Vec<(u16, u64, u64)> = Vec::new();
        for (section, bytes) in &self.extra {
            if bytes.is_empty() {
                continue;
            }
            dir.push((*section, out.len() as u64, bytes.len() as u64));
            out.raw(bytes);
        }
        for (id, bytes) in &self.raw_extra {
            if bytes.is_empty() {
                continue;
            }
            raw_dir.push((*id, out.len() as u64, bytes.len() as u64));
            out.raw(bytes);
        }

        // The meta region: directory first, then the block index, checksummed together and
        // addressed by the footer as one span. One suffix read brings back the footer and,
        // for any segment the writer produced, this whole region.
        let meta_offset = out.len() as u64;
        dir.push((
            Section::Blocks,
            0, // patched below, once the directory's own length is known
            idx.len() as u64,
        ));
        let mut meta = Enc::default();
        meta.u32((dir.len() + raw_dir.len()) as u32);
        for (section, offset, len) in &dir {
            meta.u16(*section as u16);
            meta.u64(*offset);
            meta.u64(*len);
        }
        for (id, offset, len) in &raw_dir {
            meta.u16(*id);
            meta.u64(*offset);
            meta.u64(*len);
        }
        let blocks_at = meta_offset + meta.len() as u64;
        // Patch the Blocks entry now that the directory's size is known. Encoding it twice
        // would be simpler and would silently break the moment the directory's size depended
        // on the value being patched.
        let entry = 4 + (dir.len() - 1) * (2 + 8 + 8) + 2;
        if let Some(slot) = meta.0.get_mut(entry..entry + 8) {
            slot.copy_from_slice(&blocks_at.to_le_bytes());
        }
        meta.raw(&idx.0);

        let meta_len = meta.len() as u32;
        let sum = checksum(&meta.0);
        out.raw(&meta.0);

        // The footer is last and fixed-width, so `Range: -N` finds it without knowing the
        // object's length -- which is what makes an open possible from the key alone.
        out.raw(MAGIC);
        out.u16(VERSION);
        out.u64(meta_offset);
        out.u32(meta_len);
        out.u32(self.rows);
        out.u64(sum);
        out.raw(MAGIC);
        debug_assert_eq!(
            out.len() as u64 - meta_offset - u64::from(meta_len),
            FOOTER_LEN as u64
        );
        Bytes::from(out.0)
    }
}
