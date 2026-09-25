//! Building a segment.

use crate::codec::{Enc, checksum};
use crate::{BlockMeta, Document, FOOTER_LEN, FormatError, MAGIC, Section, VERSION, Value};
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
    write_fields: bool,
    write_text_fields: bool,
    text_fields: Vec<String>,
    rows_per_block: usize,
    /// Whether sealed blocks carry zone maps. Cleared only by [`Self::seal_segment`]'s
    /// fallback, for a segment whose integer attribute names cannot fit the index.
    zone_maps: bool,
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
            write_fields: true,
            write_text_fields: true,
            text_fields: Vec::new(),
            rows_per_block: rows_per_block.max(1),
            zone_maps: true,
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

    /// Records which data row each **index** row names.
    ///
    /// ⚠️ **Written only when a row is actually duplicated.** An identity mapping says nothing
    /// a reader cannot derive, and writing it would change the bytes of every segment that
    /// does not replicate — for a feature nobody enabled.
    #[must_use]
    pub fn with_index_rows(self, rows: &[u32]) -> Self {
        if rows.iter().enumerate().all(|(i, r)| *r as usize == i) {
            return self;
        }
        let mut out = Vec::with_capacity(rows.len() * 4);
        for r in rows {
            out.extend_from_slice(&r.to_le_bytes());
        }
        self.with_section(Section::IndexRows, out)
    }

    /// Names the attribute(s) this segment's text index was built over.
    ///
    /// ⚠️ Recorded **whenever a text index exists**, including for the default name. A rule
    /// that skipped the section for `"text"` would leave the common path untested and make
    /// absence mean two different things in a newly written segment; absence is reserved for
    /// segments written before the section existed.
    #[must_use]
    pub fn with_text_fields(mut self, names: &[String]) -> Self {
        self.text_fields = names.to_vec();
        self
    }

    /// Emits a segment with **no** `TextFields` section, as one written before it existed.
    ///
    /// The sibling of [`Self::without_fields_section_for_test`] and for the same reason:
    /// forward compatibility that cannot be constructed cannot be tested, and segments are
    /// immutable, so pre-M6c segments outlive every reader that meets them.
    #[doc(hidden)]
    #[must_use]
    pub fn without_text_fields_section_for_test(mut self) -> Self {
        self.write_text_fields = false;
        self
    }

    /// Emits a segment with **no** `Fields` section, as one written before it existed.
    ///
    /// The only way to build the old shape once the writer always emits the new one, and
    /// forward compatibility that cannot be constructed cannot be tested.
    #[doc(hidden)]
    #[must_use]
    pub fn without_fields_section_for_test(mut self) -> Self {
        self.write_fields = false;
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
        if self.zone_maps {
            for d in rows {
                for (k, v) in &d.attrs {
                    if let Value::Int(n) = v {
                        // Zone maps are built here, from the rows as they are written --
                        // never from a later pass that could disagree with the bytes.
                        let e = zones.entry(k.clone()).or_insert((*n, *n));
                        e.0 = e.0.min(*n);
                        e.1 = e.1.max(*n);
                    }
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

    /// Seals the last block and emits the segment, or reports what it cannot store.
    ///
    /// ⚠️ **Refusal, not truncation.** The document model expresses named, plural and sparse
    /// fields (M3b.1); the segment layout stores one dense vector in
    /// [`crate::DEFAULT_FIELD`] until M3b.3. In between, a document the format cannot hold
    /// must fail loudly — silently writing it as nothing is what this actually did when the
    /// model landed first, and a caller cannot recover from a loss it is not told about.
    pub fn try_finish(self) -> Result<Bytes, FormatError> {
        for d in &self.docs {
            crate::check_storable(d)?;
        }
        // ⚠️ The refusal M3b's `check_storable` arm becomes, narrowed rather than deleted.
        // Its lesson was never "sparse is unsupported"; it was that a writer which accepts a
        // document and stores nothing of it is the bug. The postings are built a layer up and
        // handed over, so a writer that was not handed them is in exactly that state.
        if self.docs.iter().any(has_sparse)
            && !self
                .extra
                .iter()
                .any(|(s, b)| *s == Section::SparsePostings && !b.is_empty())
        {
            return Err(FormatError::Unsupported(
                "a sparse field was pushed but no SparsePostings section was attached: build \
                 it with `pstore_format::sparse::build` and pass it to `with_section`",
            ));
        }
        // Sealing is the only way to learn how big the meta region came out, so the width
        // check reads the result rather than predicting it from the inputs.
        let (bytes, fits) = self.seal_segment();
        if !fits {
            return Err(FormatError::Unsupported(
                "too wide to open in one round trip: the directory and field table do not \
                 fit the suffix read, so every query would cost an extra round trip",
            ));
        }
        Ok(bytes)
    }

    /// Seals the last block and emits the segment.
    ///
    /// ⚠️ Panics-free but **lossy** for anything [`Self::try_finish`] would refuse. Kept for
    /// callers that have already constructed only storable documents; new code should use
    /// `try_finish`.
    #[must_use]
    pub fn finish(self) -> Bytes {
        self.seal_segment().0
    }

    /// Seals the segment, and reports whether its meta region fits the suffix read.
    fn seal_segment(mut self) -> (Bytes, bool) {
        let docs = std::mem::take(&mut self.docs);
        // ⚠️ One set of sections per named field. Fields are taken in name order so the
        // layout is deterministic, and **field 0 keeps the legacy section ids** so a reader
        // that predates the Fields table sees exactly the single-dense view it expects
        // rather than an arbitrary field's vectors.
        let mut names: Vec<String> = docs
            .iter()
            .flat_map(|d| d.vectors.keys().cloned())
            .collect();
        names.sort();
        names.dedup();

        let mut dir: Vec<(Section, u64, u64)> = Vec::new();
        let mut fields: Vec<crate::FieldLayout> = Vec::new();
        // Each dense field's section body, written after the blocks once their size is known.
        let mut bodies: Vec<(Section, Vec<u8>)> = Vec::new();
        // ⚠️ Counted over DENSE fields only. Field 0 keeps the legacy section ids and names
        // are sorted, so counting sparse fields here would let one named `body_sparse` take
        // slot 0 from a dense `vector`: ids 2/3/4 would never be written, and a search over
        // the dense field would return zero rows with nothing reporting an error. Whether a
        // hybrid segment worked would depend on how two field names happen to sort.
        let mut fi = 0usize;
        for name in &names {
            if is_sparse(&docs, name) {
                // The postings themselves are attached by whoever built them — the format
                // stores them, it does not invert the documents here. What the segment owes
                // the field is a row saying where to look.
                fields.push(crate::FieldLayout {
                    name: name.clone(),
                    kind: 1,
                    metric: 0,
                    dims: 0,
                    per_row: 0,
                    vectors: Section::SparsePostings as u16,
                    rabitq: 0,
                    sq8: 0,
                });
                continue;
            }
            let vectors_id = if fi == 0 {
                Section::Vectors
            } else {
                Section::FieldVectors
            };
            let dims = docs
                .iter()
                .find_map(|d| d.field(name).first().map(Vec::len))
                .unwrap_or(0);
            if dims == 0 {
                continue;
            }
            fi += 1;
            // Fixed width when every row holds exactly one vector, which is the dense case
            // and the one worth keeping cheap.
            let per_row = if docs.iter().all(|d| d.field(name).len() == 1) {
                1u32
            } else {
                0
            };
            let mut body: Vec<u8> = Vec::new();
            if per_row == 0 {
                // (rows + 1) offsets, so row `i` is `offsets[i]..offsets[i + 1]` and the
                // last row needs no special case.
                let mut at = 0u64;
                let mut offsets: Vec<u64> = Vec::with_capacity(docs.len() + 1);
                for d in &docs {
                    offsets.push(at);
                    at += (d.field(name).len() * dims * 4) as u64;
                }
                offsets.push(at);
                for o in &offsets {
                    body.extend_from_slice(&o.to_le_bytes());
                }
            }
            for d in &docs {
                for v in d.field(name) {
                    for j in 0..dims {
                        body.extend_from_slice(&v.get(j).copied().unwrap_or(0.0).to_le_bytes());
                    }
                }
            }
            bodies.push((vectors_id, body));
            fields.push(crate::FieldLayout {
                name: name.clone(),
                kind: 0,
                metric: 0,
                dims: dims as u32,
                per_row,
                vectors: vectors_id as u16,
                rabitq: if fi == 1 {
                    Section::RaBitQ as u16
                } else {
                    Section::FieldRaBitQ as u16
                },
                sq8: if fi == 1 {
                    Section::Sq8 as u16
                } else {
                    Section::FieldSq8 as u16
                },
            });
        }
        // The meta region: directory first, then the block index, checksummed together and
        // addressed by the footer as one span. One suffix read brings back the footer and,
        // for any segment the writer produced, this whole region.
        // ⚠️ The Fields table lives INSIDE the meta region, beside the directory and the
        // block index, so the one suffix read that opens a segment brings it back. Written
        // into the body instead it would sit outside those bytes and cost a second round
        // trip on every cold open — for a table of a few dozen bytes that every read needs.
        let fields_bytes = if fields.is_empty() || !self.write_fields {
            Vec::new()
        } else {
            let mut t = Enc::default();
            t.u32(fields.len() as u32);
            for f in &fields {
                t.bytes(f.name.as_bytes());
                t.u8(f.kind);
                t.u8(f.metric);
                t.u32(f.dims);
                t.u32(f.per_row);
                t.u16(f.vectors);
                t.u16(f.rabitq);
                t.u16(f.sq8);
            }
            t.0
        };

        // ⚠️ Recorded only when the segment actually carries postings. A name without a
        // posting list is a segment claiming a text index it does not have, and every reader
        // that trusts `text_fields()` would then accept a query it can only answer with
        // nothing.
        let has_text = self
            .extra
            .iter()
            .any(|(s, b)| *s == Section::TextPostings && !b.is_empty());
        let text_bytes = if self.text_fields.is_empty() || !self.write_text_fields || !has_text {
            Vec::new()
        } else {
            let mut t = Enc::default();
            t.u32(self.text_fields.len() as u32);
            for n in &self.text_fields {
                t.bytes(n.as_bytes());
            }
            t.0
        };

        // ⚠️ **What the meta region holds besides the block index** (M9a, found at code
        // review). The fitting loop measured the block index against the WHOLE budget while
        // `fits` below measures the whole region, so an index within this overhead of the
        // budget passed the loop, skipped the zone-map fallback, and was refused anyway --
        // one document with ~175 integer attribute names bricked its index. Nothing in the
        // overhead depends on the block layout: one fixed-width directory entry per section,
        // plus the two tables, all known before a block is sealed.
        const ENTRY: usize = 2 + 8 + 8;
        let sections = bodies.len()
            + self.extra.iter().filter(|(_, b)| !b.is_empty()).count()
            + self.raw_extra.iter().filter(|(_, b)| !b.is_empty()).count()
            + usize::from(!fields_bytes.is_empty())
            + usize::from(!text_bytes.is_empty())
            + 1;
        let overhead = 4 + sections * ENTRY + fields_bytes.len() + text_bytes.len();
        let target = self.index_budget.saturating_sub(overhead);

        // ⚠️ Grow the block size until the index fits the budget, rather than trusting the
        // caller's request. Doubling terminates: every doubling at least halves the block
        // count, and without zone maps one block's index entry is a handful of bytes.
        //
        // ⚠️ **But not with them** (M9a, found at spec review). A zone map is keyed by an
        // integer attribute's NAME, and a block's map holds every name its rows carry -- so
        // at one block the index holds the union of the segment's names, and no block size
        // shrinks that. A batch of `{"score_<uuid>": 1}` documents made the segment
        // unwritable, and since the fold seals here too, every fold of the tenant failed.
        // So when doubling ends without fitting, seal again WITHOUT zone maps, from the
        // requested block size: a block with no map is always read (`blocks_to_read`), which
        // costs pruning and never a row. A segment that fits is sealed exactly as before.
        let requested = self.rows_per_block;
        let idx = loop {
            self.blocks.clear();
            self.body = Enc::default();
            self.rows = 0;
            for chunk in docs.chunks(self.rows_per_block) {
                self.seal(chunk);
            }
            let idx = self.encode_index();
            if idx.len() <= target {
                break idx;
            }
            if self.blocks.len() <= 1 {
                if !self.zone_maps {
                    break idx;
                }
                self.zone_maps = false;
                self.rows_per_block = requested;
                continue;
            }
            self.rows_per_block = self.rows_per_block.saturating_mul(2);
        };

        let mut out = self.body;
        for (id, body) in &bodies {
            dir.push((*id, out.len() as u64, body.len() as u64));
            out.raw(body);
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

        let meta_offset = out.len() as u64;
        // These live inside the meta region, so their offsets are not known until the
        // directory's own length is. Pushed last, in this order, and patched below.
        //
        // ⚠️ A LIST, not a pair of hardcoded cases. This loop used to special-case "one entry
        // or two", and the comment below explains why a wrong slot corrupts silently rather
        // than failing — so a third meta-region section is exactly how that happens. Adding
        // one is now appending to `inline`.
        let mut inline: Vec<(Section, &[u8])> = Vec::new();
        if !fields_bytes.is_empty() {
            inline.push((Section::Fields, &fields_bytes));
        }
        if !text_bytes.is_empty() {
            inline.push((Section::TextFields, &text_bytes));
        }
        inline.push((Section::Blocks, &idx.0));
        for (section, bytes) in &inline {
            dir.push((*section, 0, bytes.len() as u64));
        }
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
        // Patch the offsets of the entries that live inside the meta region, now that the
        // directory's own length is known. Encoding twice would be simpler and would break
        // the moment a directory's size depended on the value being patched.
        //
        // ⚠️ Entries are FIXED width — 2 + 8 + 8 — which is what makes this arithmetic
        // sound. It is also why the field names went into their own section rather than
        // into the entries: a variable-length name would make this patch land in the wrong
        // slot and corrupt silently rather than fail.
        let mut at = meta_offset + meta.len() as u64;
        for (n, (_, bytes)) in inline.iter().enumerate() {
            let entry = 4 + (dir.len() - inline.len() + n) * ENTRY + 2;
            if let Some(slot) = meta.0.get_mut(entry..entry + 8) {
                slot.copy_from_slice(&at.to_le_bytes());
            }
            at += bytes.len() as u64;
        }
        for (_, bytes) in &inline {
            meta.raw(bytes);
        }

        // ⚠️ The meta region must fit the one suffix read that opens a segment. The fitting
        // loop above can only shrink the BLOCK index; the directory and the Fields table
        // grow with the field count and it cannot help with those. Past that point a cold
        // open silently costs a second round trip — and every query built on it a fourth.
        debug_assert_eq!(
            meta.len(),
            overhead + idx.len(),
            "the overhead above is wrong"
        );
        let fits = meta.len() <= self.index_budget;

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
        (Bytes::from(out.0), fits)
    }
}

/// Whether any document carries `name` as a sparse field.
///
/// ⚠️ Asked per field rather than per document: a name used densely by one document and
/// sparsely by another is a schema the format cannot represent, and taking the first answer
/// silently picks a layout for the rest.
fn is_sparse(docs: &[Document], name: &str) -> bool {
    docs.iter()
        .any(|d| matches!(d.vectors.get(name), Some(crate::VectorField::Sparse(_))))
}

/// Whether a document carries any sparse field at all.
fn has_sparse(d: &Document) -> bool {
    d.vectors
        .values()
        .any(|f| matches!(f, crate::VectorField::Sparse(_)))
}
