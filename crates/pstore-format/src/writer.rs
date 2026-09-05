//! Building a segment.

use crate::codec::{Enc, checksum};
use crate::{BlockMeta, Document, FOOTER_LEN, MAGIC, VERSION, Value};
use bytes::Bytes;
use std::collections::BTreeMap;

/// Accumulates documents and emits one immutable segment.
#[derive(Debug)]
pub struct SegmentWriter {
    rows_per_block: usize,
    pending: Vec<Document>,
    blocks: Vec<BlockMeta>,
    body: Enc,
    rows: u32,
}

impl SegmentWriter {
    /// A writer that seals a block every `rows_per_block` documents.
    ///
    /// Block size is the one tunable that most directly trades request count against
    /// bytes: too small and a scan issues many ranged reads, too large and a filtered scan
    /// drags in rows it will discard.
    #[must_use]
    pub fn new(rows_per_block: usize) -> Self {
        Self {
            rows_per_block: rows_per_block.max(1),
            pending: Vec::new(),
            blocks: Vec::new(),
            body: Enc::default(),
            rows: 0,
        }
    }

    /// Adds a document, sealing a block when it fills.
    pub fn push(&mut self, doc: Document) {
        self.pending.push(doc);
        if self.pending.len() >= self.rows_per_block {
            self.seal();
        }
    }

    fn seal(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let offset = self.body.len() as u64;
        let mut zones: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        let mut blk = Enc::default();
        blk.u32(self.pending.len() as u32);
        for d in &self.pending {
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
            rows: self.pending.len() as u32,
            zones,
        });
        self.rows += self.pending.len() as u32;
        self.pending.clear();
    }

    /// Seals the last block and emits the segment.
    #[must_use]
    pub fn finish(mut self) -> Bytes {
        self.seal();

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
