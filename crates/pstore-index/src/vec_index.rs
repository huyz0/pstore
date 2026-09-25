//! The clustered index as it lives on the blob store.
//!
//! ## Layout, and why the rows are reordered
//!
//! A probe fetches **one posting list**, so a posting list has to be one byte range. That
//! means the rows of a segment are written in list order — the permutation is part of the
//! build, not an index on top of it. Anything else makes a probe a scatter of `n` ranges
//! and turns width back into depth.
//!
//! ```text
//! segment:   [blocks][vectors f32][rabitq][sq8][directory][footer]
//! centroids: [count][dim][centroid f32 ...][first_row, len ...]
//! ```
//!
//! ## Why the centroids are their own object
//!
//! ⚠️ Not a section of the segment. A section only saves a round trip while it fits the
//! same suffix read as the footer, and `vector-index-survey.md` sizes the production
//! centroid table at ~768 MB. As its own object it is fetched **in parallel** with the
//! footer — width, not depth — and it gets the separate cache class D-9 asks for, which
//! something buried inside a segment cannot have.

use crate::cluster::{Clustering, Params};
use crate::ladder::Ladder;
use crate::rabitq::Quantizer;
use crate::{search, sq8};
use pstore_blob::{BlobStore, Key};
use pstore_format::{Document, Section, Segment, SegmentWriter};

/// Below this many rows an index is scanned exactly and no centroids are written (D-10).
///
/// ⚠️ Stated as a number so criterion 11 can be checked. It is below
/// `vector-index-survey.md`'s ~50k–200k, which is quoted for 128 dimensions; a higher
/// dimension scans more bytes per row, so the crossover moves down. Evidence for OQ-36.
pub const EXACT_SCAN_THRESHOLD: usize = 25_000;

/// What a query is willing to pay.
#[derive(Debug, Clone, Copy)]
pub struct Query {
    /// Results wanted.
    pub k: usize,
    /// Posting lists to probe. Free in depth, **linear in bytes**.
    pub p: usize,
    /// Rung-0 candidates kept per result. Free in **both** — they were already scored.
    pub oversample: usize,
    /// Which rung the answer comes from.
    pub rerank: Rerank,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            k: 10,
            // ⚠️ 8, measured, not 16. Recall is flat from p=4 on clustered data
            // (0.980 / 0.981 / 0.981 at p = 4 / 8 / 16) while bytes are linear in it
            // (0.48 / 0.84 / 1.60 MB), so 16 was 3.3x over-provisioned — chosen as the
            // "mid-range" of the corpus's 8-64, which is a range stated for a
            // billion-vector index.
            //
            // 8 rather than the measured-sufficient 4 because the corpus these numbers come
            // from is a Gaussian mixture, which is the case clustering handles best; taking
            // the exact minimum that works on the flattering case is fitting the default to
            // the generator. 8 is a 2x margin over what was needed and still halves the
            // bytes.
            //
            // ⚠️ Bytes per query is the cost model's dominant input — `cost-model.md` prices
            // a node on vectors scanned per second, with a 75x swing across scan sizes — so
            // this is not a minor tuning change.
            p: 8,
            // Measured: 8 gives 0.801, 32 gives 0.981, and it costs nothing.
            oversample: 32,
            // ⚠️ `Fast`, not `None`. C-3: rung 0 alone measures 0.30 recall@10, not the
            // 90-95% `quantization.md` claims, and int8 rides in the same round trip.
            rerank: Rerank::Fast,
        }
    }
}

/// How much precision a query buys (D-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rerank {
    /// 1-bit codes only. Cheapest, and not the default: it measures 0.30 recall@10.
    None,
    /// int8 over the survivors, **in the same round trip** as the 1-bit codes.
    Fast,
    /// Full precision over the survivors. One extra round trip, on request.
    Exact,
}

/// The centroid table: vectors, and where each list's rows begin.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Centroids {
    /// One vector per posting list.
    pub vectors: Vec<Vec<f32>>,
    /// `(first_row, len)` per posting list, into the segment's row order.
    pub spans: Vec<(u32, u32)>,
}

impl Centroids {
    /// Encodes the table.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let dim = self.vectors.first().map_or(0, Vec::len);
        let mut out = Vec::with_capacity(8 + self.vectors.len() * (dim * 4 + 8));
        out.extend_from_slice(&(self.vectors.len() as u32).to_le_bytes());
        out.extend_from_slice(&(dim as u32).to_le_bytes());
        for v in &self.vectors {
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        for (first, len) in &self.spans {
            out.extend_from_slice(&first.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
        }
        out
    }

    /// Decodes a table, returning `None` for anything malformed.
    ///
    /// A short or ragged table would otherwise produce centroids of the wrong dimension and
    /// spans pointing at the wrong rows — a wrong answer rather than an error.
    #[must_use]
    pub fn decode(raw: &[u8]) -> Option<Self> {
        let n = u32::from_le_bytes(raw.get(0..4)?.try_into().ok()?) as usize;
        let dim = u32::from_le_bytes(raw.get(4..8)?.try_into().ok()?) as usize;
        let mut at: usize = 8;
        let mut vectors = Vec::with_capacity(n);
        for _ in 0..n {
            let end = at.checked_add(dim * 4)?;
            let v = raw
                .get(at..end)?
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap_or([0; 4])))
                .collect();
            vectors.push(v);
            at = end;
        }
        let mut spans = Vec::with_capacity(n);
        for _ in 0..n {
            let end = at.checked_add(8)?;
            let s = raw.get(at..end)?;
            spans.push((
                u32::from_le_bytes(s.get(0..4)?.try_into().ok()?),
                u32::from_le_bytes(s.get(4..8)?.try_into().ok()?),
            ));
            at = end;
        }
        Some(Self { vectors, spans })
    }
}

/// A built index: the segment bytes, and the centroid table if one was worth building.
#[derive(Debug)]
pub struct Built {
    /// The segment object.
    pub segment: bytes::Bytes,
    /// `None` below [`EXACT_SCAN_THRESHOLD`] — the index is scanned exactly (D-10).
    pub centroids: Option<Centroids>,
    /// Documents in the order they were written, which is list order when clustered.
    pub order: Vec<usize>,
    /// The term dictionary sidecar, when a text field was built alongside.
    pub text_dictionary: Option<Vec<u8>>,
    /// The sparse dictionary sidecar, when a sparse field was built alongside.
    ///
    /// ⚠️ Built here rather than by the caller because the postings address **segment rows**,
    /// and the segment's row order is decided by the clustering a few lines above. A caller
    /// transposing the unreordered documents would produce a posting list that is internally
    /// consistent and points at the wrong documents.
    pub dictionary: Option<Vec<u8>>,
}

/// Builds a segment and, above the threshold, its clustered index.
///
/// ⚠️ **One switch, at one place.** Below the threshold no centroid object is written at
/// all — not an empty one — so "is this index clustered?" is answered by whether the object
/// exists, and there is no second code path that could disagree with the first.
#[must_use]
pub fn build(docs: &[Document], params: Params) -> Built {
    build_field(docs, params, pstore_format::DEFAULT_FIELD)
}

/// One document's vector in `field`, at exactly `dim` components.
///
/// ⚠️ **The field's width is defined once, here.** `dim` comes from the first document that
/// carries the field, and the segment writer pads or truncates to the same number — so
/// letting a differently-sized vector through to the clustering is not a smaller problem
/// than letting it through to the layout. A ragged corpus makes farthest-point seeding pick
/// the outlier as a centroid, the centroid table becomes ragged, `Centroids::encode` writes
/// one `dim` for all of them, and the spans decode to garbage: measured as a request for
/// bytes `16477954617..457680856537` of a 218 KB object.
fn fit(d: &Document, field: &str, dim: usize) -> Vec<f32> {
    let mut v = d.field(field).first().cloned().unwrap_or_default();
    v.resize(dim, 0.0);
    v
}

/// Builds a segment and index over one **named** vector field.
///
/// ⚠️ The field is a parameter rather than an assumption. A document may carry a body
/// embedding and a title embedding of different dimensions, so "build the index" is not a
/// well-formed instruction without saying which.
#[must_use]
pub fn build_field(docs: &[Document], params: Params, field: &str) -> Built {
    build_hybrid(docs, params, field, None)
}

/// Builds a segment carrying a dense field **and** a sparse one.
///
/// ⚠️ One function, because the two are not independent: the dense clustering decides the
/// segment's row order and the sparse postings address those rows.
#[must_use]
pub fn build_hybrid(
    docs: &[Document],
    params: Params,
    field: &str,
    sparse_field: Option<&str>,
) -> Built {
    build_all(docs, params, field, sparse_field, None)
}

/// The key of a segment's centroid table.
///
/// ⚠️ Derived, never discovered — the third instance of a convention `text::dict_key`
/// (`.tdict`) and `sparse::dict_key` (`.sdict`) already set. Absent in the store is not an
/// error: D-10 reads that as "this index is below the exact-scan threshold, scan me exactly".
#[must_use]
pub fn centroid_key(segment: &Key) -> Key {
    Key::new(format!("{}.cen", segment.as_str()))
}

/// [`build_all`], refusing what the format cannot store instead of writing it as nothing.
///
/// ⚠️ **The fold needs this and a test fixture does not.** `build_all` uses `finish`, and its
/// comment is right for its own caller: a fixture that has already read every document wants
/// the lossy form, and a width error reported there is reported at the wrong layer. But
/// `Engine::seal`'s comment records the opposite lesson from the other end — *"`finish` is
/// lossy for anything the format cannot hold and the fold used it, so a document the writer
/// could not store was written as nothing and reported as durable"*. The refusal this keeps is
/// `try_finish`'s **index-budget** one, "too wide to open in one round trip", which
/// `check_storable` at the write door knows nothing about.
///
/// # Errors
/// If the segment cannot be stored faithfully or would not open in one round trip.
pub fn try_build_all(
    docs: &[Document],
    params: Params,
    field: &str,
    sparse_field: Option<&str>,
    text_field: Option<&str>,
) -> Result<Built, pstore_format::FormatError> {
    let (w, centroids, order, dictionary, text_dictionary) =
        assemble(docs, params, field, sparse_field, text_field);
    Ok(Built {
        segment: w.try_finish()?,
        centroids,
        order,
        dictionary,
        text_dictionary,
    })
}

/// Builds a segment carrying a dense field, a sparse one, and a text one.
///
/// ⚠️ One function, because none of the three is independent: the dense clustering decides the
/// segment's row order, and both the sparse postings and the text postings **and fieldnorms**
/// address those rows. Three builders called separately over the input order produce three
/// internally consistent indexes that point at three different documents.
#[must_use]
pub fn build_all(
    docs: &[Document],
    params: Params,
    field: &str,
    sparse_field: Option<&str>,
    text_field: Option<&str>,
) -> Built {
    let (w, centroids, order, dictionary, text_dictionary) =
        assemble(docs, params, field, sparse_field, text_field);
    Built {
        // ⚠️ `finish`, not `try_finish`, and deliberately: `build` has already read every
        // document through `d.vector()`, so anything the format cannot store was lost before
        // this point. Refusing here would report the right error at the wrong layer. The
        // caller that needs the refusal is the FOLD, and it has `try_build_all`.
        segment: w.finish(),
        centroids,
        order,
        dictionary,
        text_dictionary,
    }
}

/// Everything both builders share: the clustering, the codes, and the sidecars — stopping one
/// step short of sealing, because that step is the only thing they disagree about.
#[expect(
    clippy::type_complexity,
    reason = "the tuple exists so the two builders cannot drift; naming it would be a struct \
              that is `Built` minus the one field they differ on"
)]
fn assemble(
    docs: &[Document],
    params: Params,
    field: &str,
    sparse_field: Option<&str>,
    text_field: Option<&str>,
) -> (
    SegmentWriter,
    Option<Centroids>,
    Vec<usize>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
) {
    let dim = docs
        .iter()
        .find_map(|d| d.field(field).first().map(Vec::len))
        .unwrap_or(0);
    let quantizer = Quantizer::new(dim.max(1));

    // `home` is the list each INDEX row sits in, beside `order`; empty when unclustered.
    let (order, home, centroids) = if docs.len() < params.exact_scan_threshold || dim == 0 {
        ((0..docs.len()).collect::<Vec<_>>(), Vec::new(), None)
    } else {
        let corpus: Vec<Vec<f32>> = docs.iter().map(|d| fit(d, field, dim)).collect();
        let c = Clustering::build(&corpus, params);
        // ⚠️ Rows are written in LIST order, so a posting list is one contiguous byte range
        // and a probe is one ranged read rather than a scatter of thousands.
        let mut order = Vec::with_capacity(docs.len());
        let mut home = Vec::with_capacity(docs.len());
        let mut spans = Vec::with_capacity(c.lists().len());
        for (li, list) in c.lists().iter().enumerate() {
            spans.push((order.len() as u32, list.len() as u32));
            order.extend(list.iter().copied());
            home.extend(std::iter::repeat_n(li, list.len()));
        }
        (
            order,
            home,
            Some(Centroids {
                vectors: c.centroids().to_vec(),
                spans,
            }),
        )
    };

    // ⚠️ **Two row spaces.** `order` is list order and repeats a boundary vector once per list
    // it was replicated into; `primary` is each document once, in the order it first appears.
    // Codes stride by `order`, blocks and both sidecars by `primary`, and `index_rows` maps
    // one to the other. Writing the blocks over `order` is what made 400 documents come back
    // as 431 rows and compound on every merge.
    let mut primary: Vec<usize> = Vec::with_capacity(order.len());
    let mut slot_of: std::collections::HashMap<usize, u32> = std::collections::HashMap::new();
    let mut index_rows: Vec<u32> = Vec::with_capacity(order.len());
    for row in &order {
        let slot = *slot_of.entry(*row).or_insert_with(|| {
            primary.push(*row);
            (primary.len() - 1) as u32
        });
        index_rows.push(slot);
    }

    let mut w = SegmentWriter::new(64);
    let mut rabitq = Vec::new();
    let mut eights = Vec::new();
    let zero = vec![0.0f32; dim];
    for row in &primary {
        if let Some(d) = docs.get(*row) {
            w.push(d.clone());
        }
    }
    for (slot, row) in order.iter().enumerate() {
        let Some(d) = docs.get(*row) else { continue };
        if dim > 0 {
            // The centroid a row is coded against: its own LIST's, or the origin when
            // unclustered. ⚠️ Per index row, not per document. A replicated document has one
            // copy per list, and search scores each copy with its list's `<c, q>`. Keyed by
            // document, the last list won and every other copy scored `<c_that - c_last, q>`
            // off -- up or down, so the best copy was not the most accurate one.
            let cen = home
                .get(slot)
                .and_then(|li| centroids.as_ref()?.vectors.get(*li))
                .unwrap_or(&zero);
            // ⚠️ Zero-filled when the document lacks this field, never skipped. Codes are
            // fixed width per row, so a skipped row shifts every later one — and the
            // previous version silently swallowed the resulting dimension error with
            // `if let Ok`, producing a segment whose code section was EMPTY and an index
            // that returned nothing at all.
            let v = fit(d, field, dim);
            match quantizer.encode_residual(&v, cen) {
                Ok(code) => code.write_to(&mut rabitq),
                // Unreachable: `v` is `dim` long by construction. Filled rather than
                // skipped so the row stride stays right even if that ever stops holding.
                Err(_) => rabitq.extend(std::iter::repeat_n(0u8, quantizer.code_len())),
            }
            sq8::write_to(&sq8::encode(&v), &mut eights);
        }
    }
    // ⚠️ Transposed over the documents **in segment row order**, not in input order — and over
    // `primary`, not `order`. Over `order` a replicated document is counted twice in
    // `doc_count` and in every one of its terms' `df`, which is a corpus scored against
    // statistics that say it is bigger than it is. Live since M3 and caught by nothing: the
    // recall gate builds no text field and the ndcg gate does not replicate.
    let rows: Vec<Document> = primary
        .iter()
        .filter_map(|r| docs.get(*r).cloned())
        .collect();
    let sparse = sparse_field.map(|name| {
        pstore_format::sparse::build(&rows, name, pstore_format::sparse::DEFAULT_ENCODING)
    });
    let text = text_field.map(|name| pstore_format::text::build(&rows, name));
    let dictionary = sparse.as_ref().map(|p| p.dictionary.clone());
    let text_dictionary = text.as_ref().map(|t| t.dictionary.clone());
    // ⚠️ **Between `RaBitQ` and `Sq8`, and the order is load-bearing.** Body sections are laid
    // out in attachment order, and the coalescer merges nearby ranges — so with the mapping
    // *after* `Sq8`, a rung-0 query fetching rabitq and the mapping bridges straight across
    // `Sq8` and drags in the int8 bytes rung 0 exists to avoid. Measured: 44,784 of them, by
    // `a_rung_zero_query_reads_no_int8_or_float_bytes`.
    let mut w = w
        .with_section(Section::RaBitQ, rabitq)
        .with_index_rows(&index_rows)
        .with_section(Section::Sq8, eights);
    if let Some(p) = sparse {
        w = w.with_section(Section::SparsePostings, p.section);
    }
    if let Some(t) = text {
        w = w
            .with_section(Section::TextPostings, t.postings)
            .with_section(
                Section::Fieldnorms,
                pstore_format::text::encode_norms(&t.fieldnorms),
            );
        // ⚠️ Recorded, not assumed. `build_all` is already told the attribute; a segment that
        // does not carry the name leaves every reader comparing against the constant, which
        // answers a `text` query with this field's ranking and says nothing.
        if let Some(name) = text_field {
            w = w.with_text_fields(&[name.to_owned()]);
        }
    }

    // `order` as the caller sees it is the DATA row order: what `scan` returns.
    (w, centroids, primary, dictionary, text_dictionary)
}

/// An opened index: the segment's directory and the centroid table, both in memory.
#[derive(Debug)]
pub struct VecIndex {
    segment: Segment,
    centroids: Option<Centroids>,
    dim: usize,
}

impl VecIndex {
    /// Pull an index's metadata into cache without querying it — **shadow warming** (D-44).
    ///
    /// ⚠️ Deliberately **identical to `open`, with the result dropped**, and that is the whole
    /// design rather than a shortcut. `open` already reads the segment index section (`Meta`)
    /// and the centroid table (`Pinned`) and **nothing else** — the two classes D-44 says to
    /// warm, which are 0.1–1% of an index's bytes and unblock every query on it. Anything
    /// more would be a cache fill, which device endurance caps at ~67 MB/s and which
    /// `disk-space-management.md` says never to do for bulk.
    ///
    /// Expressing it as `open` means the two cannot drift: a future change that puts a bulk
    /// read into `open` fails `warming_fetches_no_bulk_bytes`, rather than silently turning
    /// every warm-up into a fill.
    ///
    /// ⚠️ **Off the query path.** This returns a future and the caller decides whether to
    /// await it; on a placement change the new owner warms while the previous owner is still
    /// serving. Awaiting it *before* the first query is what a test does to measure the dip,
    /// not what a deployment does to create one.
    pub async fn warm<S: BlobStore>(
        store: &S,
        segment: &Key,
        centroids: &Key,
    ) -> Result<(), pstore_format::FormatError> {
        // `dim` is irrelevant to which bytes are fetched; it only shapes the decoded view.
        Self::open(store, segment, centroids, 1).await.map(|_| ())
    }

    /// Opens an index. **One round trip**: the centroid object and the segment footer are
    /// different objects, so they are fetched together.
    pub async fn open<S: BlobStore>(
        store: &S,
        segment: &Key,
        centroids: &Key,
        dim: usize,
    ) -> Result<Self, pstore_format::FormatError> {
        // ⚠️ `join`, not two awaits. Both keys are derived, neither depends on the other's
        // contents, and awaiting the footer first would make every cold open two hops for
        // no reason a caller could see.
        // ⚠️ `get_immutable`, not `get`: a centroid table is written once and never changes,
        // and saying so is what lets it be cached at all — plain `get` is never cached,
        // because that is also how the mutable lane registry is read. `Pinned` because every
        // vector query needs this and it unblocks everything downstream (D-21).
        let (seg, cen) = futures_util::future::join(
            Segment::open(store, segment),
            store.get_immutable(centroids, pstore_blob::Class::Pinned),
        )
        .await;
        Ok(Self {
            segment: seg?,
            // A missing centroid object is not an error: it is how an index below the
            // threshold says "scan me exactly" (D-10).
            centroids: cen.ok().and_then(|b| Centroids::decode(b.as_ref())),
            dim,
        })
    }

    /// An index over an **already-open** segment.
    ///
    /// ⚠️ Exists so a hybrid query opens the segment once. Two retrievers each calling `open`
    /// is one extra suffix read and one extra `Meta` admission — and with a cache in the
    /// stack it is invisible, because singleflight collapses the identical concurrent reads
    /// and the request counter reports the right answer for the wrong code.
    #[must_use]
    pub fn from_parts(segment: Segment, centroids: Option<&[u8]>, dim: usize) -> Self {
        Self {
            segment,
            // A missing or unreadable centroid table is not an error: it is how an index
            // below the threshold says "scan me exactly" (D-10).
            centroids: centroids.and_then(Centroids::decode),
            dim,
        }
    }

    /// Whether this index has a clustered structure, or is scanned exactly.
    #[must_use]
    pub fn is_clustered(&self) -> bool {
        self.centroids.is_some()
    }

    /// The opened segment.
    #[must_use]
    pub fn segment(&self) -> &Segment {
        &self.segment
    }

    /// Nearest neighbours. **One further round trip**, whatever `p` is.
    /// Nearest neighbours in a **named** field.
    ///
    /// An absent field is an error: a miss returning zero hits is indistinguishable from a
    /// field with no matches, so a caller could not tell a typo from data.
    pub async fn search_field<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        field: &str,
        query: &[f32],
        q: Query,
    ) -> Result<Vec<(usize, f32)>, pstore_format::FormatError> {
        if self.segment.field_layout(field).is_none() {
            return Err(pstore_format::FormatError::UnknownField);
        }
        self.search(store, key, query, q).await
    }

    /// Nearest neighbours in the index's own field. **One further round trip**, whatever
    /// `p` is.
    ///
    /// Prefer [`Self::search_field`], which names the field and errors on a miss; this is
    /// the single-field convenience it is built on.
    pub async fn search<S: BlobStore>(
        &self,
        store: &S,
        key: &Key,
        query: &[f32],
        q: Query,
    ) -> Result<Vec<(usize, f32)>, pstore_format::FormatError> {
        let quantizer = Quantizer::new(self.dim.max(1));
        let Some(centroids) = &self.centroids else {
            // D-10: no index, so read the vectors and be exactly right.
            let vectors = self.segment.vectors(store, key).await?;
            let mut scored: Vec<(usize, f32)> = vectors
                .iter()
                .enumerate()
                .map(|(i, v)| (i, dot(v, query)))
                .collect();
            scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            scored.truncate(q.k);
            return Ok(scored);
        };

        let clustering = Clustering::from_parts(centroids.vectors.clone(), Vec::new());
        let probes = search::probe(&clustering, query, q.p);
        let code_len = quantizer.code_len();
        let (Some(rabitq), Some(sq8_span)) = (
            self.segment.section(Section::RaBitQ),
            self.segment.section(Section::Sq8),
        ) else {
            return Ok(Vec::new());
        };

        // ⚠️ The rabitq AND sq8 ranges for every probed list, issued in ONE call. That is
        // what keeps `rerank: fast` inside three round trips: both spans are known the
        // moment the probe set is, so int8 costs bytes rather than a fourth hop.
        let mut ranges = Vec::with_capacity(probes.len() * 2);
        let mut rows = Vec::with_capacity(probes.len());
        for li in &probes {
            let Some((first, len)) = centroids.spans.get(*li).copied() else {
                continue;
            };
            let (first, len) = (first as u64, len as u64);
            ranges.push(
                rabitq.start + first * code_len as u64
                    ..rabitq.start + (first + len) * code_len as u64,
            );
            rows.push((*li, first, len));
        }
        let want_sq8 = q.rerank != Rerank::None;
        if want_sq8 {
            let per = sq8::record_len(self.dim) as u64;
            for (_, first, len) in &rows {
                ranges.push(sq8_span.start + first * per..sq8_span.start + (first + len) * per);
            }
        }
        // ⚠️ The index-row mapping rides in the SAME call, one range per probed list. It is
        // only needed when the segment has two row spaces, and fetching it separately would
        // cost a fourth round trip for 4 bytes a row.
        let map_span = self.segment.section(Section::IndexRows);
        if let Some(m) = &map_span {
            for (_, first, len) in &rows {
                ranges.push(m.start + first * 4..m.start + (first + len) * 4);
            }
        }
        let bufs = store.get_ranges(key, &ranges).await?;
        // Index row -> data row, for the probed rows only.
        let mut data_row: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        if map_span.is_some() {
            let base = rows.len() * if want_sq8 { 2 } else { 1 };
            for (n, (_, first, len)) in rows.iter().enumerate() {
                let Some(buf) = bufs.get(base + n) else {
                    continue;
                };
                for i in 0..*len as usize {
                    let Some(raw) = buf.get(i * 4..(i + 1) * 4) else {
                        continue;
                    };
                    if let Ok(b) = <[u8; 4]>::try_from(raw) {
                        data_row.insert(*first as usize + i, u32::from_le_bytes(b) as usize);
                    }
                }
            }
        }

        let Ok(prepared) = quantizer.prepare(query) else {
            return Err(pstore_format::FormatError::DimensionMismatch {
                expected: self.dim,
                got: query.len(),
            });
        };
        // One `<c, q>` per probed list, not per candidate.
        let dots: Vec<f32> = rows
            .iter()
            .map(|(li, _, _)| centroids.vectors.get(*li).map_or(0.0, |c| dot(c, query)))
            .collect();

        let mut scored: Vec<(usize, f32)> = Vec::new();
        for (n, (_, first, len)) in rows.iter().enumerate() {
            let Some(buf) = bufs.get(n) else { continue };
            for i in 0..*len as usize {
                let Some(raw) = buf.get(i * code_len..(i + 1) * code_len) else {
                    continue;
                };
                let Some(code) = quantizer.read_code(raw) else {
                    continue;
                };
                let row = *first as usize + i;
                scored.push((
                    row,
                    quantizer.estimate_residual(
                        &code,
                        &prepared,
                        dots.get(n).copied().unwrap_or(0.0),
                    ),
                ));
            }
        }

        // ⚠️ **Deduplicated by DATA row, before the ladder.** A vector replicated into two
        // lists is scored once per probed list it sits in, so without this it occupies two
        // slots of one top-k and displaces a real neighbour. Before the ladder rather than
        // after, because the rerank rungs read `Sq8` and `Vectors` at INDEX rows — mapping
        // early would send them to the wrong offsets — and deduplicating after `k` has been
        // truncated returns fewer than `k`.
        if !data_row.is_empty() {
            scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
            scored.retain(|(row, _)| seen.insert(*data_row.get(row).unwrap_or(row)));
        }

        let ladder = Ladder::new(q.k, q.oversample);
        let top = ladder.rung0(scored);
        let ranked: Vec<(usize, f32)> = match q.rerank {
            Rerank::None => top.into_iter().take(q.k).collect(),
            Rerank::Fast => {
                let per = sq8::record_len(self.dim);
                let refined = ladder.rung1(&top, |row| {
                    // Find the fetched sq8 buffer holding this row.
                    rows.iter()
                        .enumerate()
                        .find(|(_, (_, first, len))| {
                            row >= *first as usize && row < (*first + *len) as usize
                        })
                        .and_then(|(n, (_, first, _))| {
                            let buf = bufs.get(rows.len() + n)?;
                            let i = row - *first as usize;
                            let raw = buf.get(i * per..(i + 1) * per)?;
                            Some(sq8::estimate_raw(raw, query))
                        })
                        .unwrap_or(f32::NEG_INFINITY)
                });
                refined.into_iter().take(q.k).collect()
            }
            Rerank::Exact => {
                // The fourth round trip, and only here: which float32 rows to read cannot
                // be known until rung 0 has ranked.
                //
                // ⚠️ Only the SURVIVORS' rows. Reading the whole section to score a few
                // hundred of them costs the segment's entire bandwidth -- 60x the bytes at
                // gate scale -- and it shipped that way, because the byte ceiling was
                // asserted only at the default rerank mode.
                // ⚠️ Mapped to DATA rows first: `Vectors` is the one code-adjacent section
                // that strides by `row_count`, because at 1,536 bytes a row duplicating it
                // would spend exactly the storage replication is supposed to save.
                let rows: Vec<usize> = top
                    .iter()
                    .map(|(r, _)| data_row.get(r).copied().unwrap_or(*r))
                    .collect();
                let vectors = self.segment.vector_rows(store, key, &rows).await?;
                ladder.rung2(&top, |row| {
                    vectors
                        .get(data_row.get(&row).unwrap_or(&row))
                        .map_or(f32::NEG_INFINITY, |v| dot(v, query))
                })
            }
        };
        // ⚠️ **Index rows become data rows only here, at the return.** Everything above reads
        // `RaBitQ`, `Sq8` and `Vectors`, all of which stride by the index row count; mapping
        // any earlier sends them to the wrong offsets. The mapping is injective after the
        // deduplication above, so this cannot collapse two hits into one.
        Ok(ranked
            .into_iter()
            .map(|(row, score)| (data_row.get(&row).copied().unwrap_or(row), score))
            .collect())
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
