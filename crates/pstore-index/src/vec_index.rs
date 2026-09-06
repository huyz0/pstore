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
}

/// Builds a segment and, above the threshold, its clustered index.
///
/// ⚠️ **One switch, at one place.** Below the threshold no centroid object is written at
/// all — not an empty one — so "is this index clustered?" is answered by whether the object
/// exists, and there is no second code path that could disagree with the first.
#[must_use]
pub fn build(docs: &[Document], params: Params) -> Built {
    let dim = docs.first().map_or(0, |d| d.vector().len());
    let quantizer = Quantizer::new(dim.max(1));

    let (order, centroids) = if docs.len() < params.exact_scan_threshold || dim == 0 {
        ((0..docs.len()).collect::<Vec<_>>(), None)
    } else {
        let corpus: Vec<Vec<f32>> = docs.iter().map(|d| d.vector().to_vec()).collect();
        let c = Clustering::build(&corpus, params);
        // ⚠️ Rows are written in LIST order, so a posting list is one contiguous byte range
        // and a probe is one ranged read rather than a scatter of thousands.
        let mut order = Vec::with_capacity(docs.len());
        let mut spans = Vec::with_capacity(c.lists().len());
        for list in c.lists() {
            spans.push((order.len() as u32, list.len() as u32));
            order.extend(list.iter().copied());
        }
        (
            order,
            Some(Centroids {
                vectors: c.centroids().to_vec(),
                spans,
            }),
        )
    };

    let mut w = SegmentWriter::new(64);
    let mut rabitq = Vec::new();
    let mut eights = Vec::new();
    // The centroid a row is coded against: its own list's, or the origin when unclustered.
    let mut home: Vec<usize> = vec![usize::MAX; docs.len()];
    if let Some(c) = &centroids {
        for (li, (first, len)) in c.spans.iter().enumerate() {
            for slot in *first..*first + *len {
                if let Some(row) = order.get(slot as usize)
                    && let Some(slot) = home.get_mut(*row)
                {
                    *slot = li;
                }
            }
        }
    }
    let zero = vec![0.0f32; dim];
    for row in &order {
        let Some(d) = docs.get(*row) else { continue };
        w.push(d.clone());
        if dim > 0 {
            let cen = centroids
                .as_ref()
                .and_then(|c| c.vectors.get(*home.get(*row).unwrap_or(&usize::MAX)))
                .unwrap_or(&zero);
            if let Ok(code) = quantizer.encode_residual(d.vector(), cen) {
                code.write_to(&mut rabitq);
            }
            sq8::write_to(&sq8::encode(d.vector()), &mut eights);
        }
    }
    Built {
        segment: w
            .with_section(Section::RaBitQ, rabitq)
            .with_section(Section::Sq8, eights)
            .finish(),
        centroids,
        order,
    }
}

/// An opened index: the segment's directory and the centroid table, both in memory.
#[derive(Debug)]
pub struct VecIndex {
    segment: Segment,
    centroids: Option<Centroids>,
    dim: usize,
}

impl VecIndex {
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
        let (seg, cen) =
            futures_util::future::join(Segment::open(store, segment), store.get(centroids)).await;
        Ok(Self {
            segment: seg?,
            // A missing centroid object is not an error: it is how an index below the
            // threshold says "scan me exactly" (D-10).
            centroids: cen.ok().and_then(|b| Centroids::decode(b.as_ref())),
            dim,
        })
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
        let bufs = store.get_ranges(key, &ranges).await?;

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

        let ladder = Ladder::new(q.k, q.oversample);
        let top = ladder.rung0(scored);
        Ok(match q.rerank {
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
                let rows: Vec<usize> = top.iter().map(|(r, _)| *r).collect();
                let vectors = self.segment.vector_rows(store, key, &rows).await?;
                ladder.rung2(&top, |row| {
                    vectors
                        .get(&row)
                        .map_or(f32::NEG_INFINITY, |v| dot(v, query))
                })
            }
        })
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
