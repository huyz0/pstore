//! Building a segment.

use crate::codec::{Enc, checksum};
use crate::{BlockMeta, Document, FOOTER_LEN, MAGIC, VERSION, Value};
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
        let mut blk = Enc::default();
        blk.u32(rows.len() as u32);
        for d in rows {
            blk.bytes(d.id.as_bytes());
            blk.u32(d.vector.len() as u32);
            for f in &d.vector {
                blk.f32(*f);
            }
            blk.u32(d.attrs.len() as u32);
            for (k, v) in &d.attrs {
                blk.bytes(k.as_bytes());
                match v {
                    Value::Int(n) => {
                        blk.u8(0);
                        blk.i64(*n);
                        // Zone maps are built here, from the rows as they are written --
                        // never from a later pass that could disagree with the bytes.
                        let e = zones.entry(k.clone()).or_insert((*n, *n));
                        e.0 = e.0.min(*n);
                        e.1 = e.1.max(*n);
                    }
                    Value::Str(s) => {
                        blk.u8(1);
                        blk.bytes(s.as_bytes());
                    }
                }
            }
        }
        let len = blk.len() as u32;
        self.body.raw(&blk.0);
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

        let index_offset = self.body.len() as u64;
        let index_len = idx.len() as u32;
        let sum = checksum(&idx.0);

        let mut out = self.body;
        out.raw(&idx.0);
        // The footer is last and fixed-width, so `Range: -N` finds it without knowing the
        // object's length -- which is what makes an open possible from the key alone.
        out.raw(MAGIC);
        out.u16(VERSION);
        out.u64(index_offset);
        out.u32(index_len);
        out.u32(self.rows);
        out.u64(sum);
        out.raw(MAGIC);
        debug_assert_eq!(
            out.len() as u64 - index_offset - u64::from(index_len),
            FOOTER_LEN as u64
        );
        Bytes::from(out.0)
    }
}
