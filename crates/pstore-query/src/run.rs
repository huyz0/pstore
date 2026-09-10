//! Running the legs — concurrently, over one open segment.

use crate::fuse::{Fusion, Hit, fuse};
use pstore_blob::{BlobStore, Key};
use pstore_format::{FormatError, Segment};
use pstore_index::sparse::SparseIndex;
use pstore_index::text::{Stats, TextIndex};
use pstore_index::vec_index::{self, VecIndex};

/// One retriever's request (D-73).
#[derive(Debug, Clone)]
pub enum Prefetch {
    /// Dense ANN over a named vector field.
    Dense {
        /// The field to search.
        field: String,
        /// The query vector.
        query: Vec<f32>,
        /// Rows this leg contributes at most.
        limit: usize,
        /// What the leg is willing to pay.
        tune: vec_index::Query,
    },
    /// Exact sparse retrieval over a named sparse field.
    Sparse {
        /// The field to search.
        field: String,
        /// `(dimension, impact)` pairs.
        query: Vec<(u32, f32)>,
        /// Rows this leg contributes at most.
        limit: usize,
    },
    /// BM25 over the segment's text field.
    ///
    /// ⚠️ `query` is a **`String`**, analyzed at query time. D-73's premise is that the
    /// request shape is the hardest thing to change, so the variant that shipped in M5b
    /// before the retriever existed is the variant that runs now — unchanged.
    Text {
        /// The field to search.
        field: String,
        /// The query text.
        query: String,
        /// Rows this leg would contribute.
        limit: usize,
    },
    /// Trigram regex — **in the shape, not in the engine**.
    ///
    /// ⚠️ Present so a caller's request does not have to change when the retriever arrives
    /// (D-73), and refused **by name** when executed. Dropping it silently would answer with a
    /// plausible ranking computed from fewer retrievers than were asked for, and nothing
    /// anywhere would say so. `full-text-search.md` names trigram as "the same inverted
    /// machinery", which is why it is the shape's next occupant rather than an invention.
    Trigram {
        /// The field to search.
        field: String,
        /// The pattern to match.
        pattern: String,
        /// Rows this leg would contribute.
        limit: usize,
    },
}

/// What can go wrong running a query.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// A retriever in the request shape that this build cannot run.
    #[error("retriever not implemented: {0}")]
    Unimplemented(&'static str),
    /// The segment, the dictionary or a field.
    #[error(transparent)]
    Format(#[from] FormatError),
}

/// One segment and the centroid table that belongs to it.
///
/// ⚠️ **Paired, never a shared centroids key.** A centroid table records *that* segment's
/// cluster assignments, so pointing every segment's dense leg at one table returns another
/// segment's clusters applied to these rows — a wrong candidate set that still answers.
/// `VecIndex::open` already takes the two together, which is where the pairing belongs.
#[derive(Debug, Clone)]
pub struct Target {
    /// The segment object.
    pub segment: Key,
    /// Its centroid table. Absent in the store is not an error: D-10 reads that as "scan me
    /// exactly".
    pub centroids: Key,
}

/// Which sidecars a query needs, derived from the legs.
struct Opened {
    segment: Segment,
    centroids: Option<bytes::Bytes>,
    dictionary: Option<bytes::Bytes>,
    terms: Option<bytes::Bytes>,
}

/// Runs every leg over one segment and fuses the answers.
///
/// ⚠️ **One open, and the legs are joined.** Each leg opening the segment for itself is one
/// extra suffix read and one extra `Meta` admission per retriever — and with a cache in the
/// stack it is invisible, because singleflight collapses the identical concurrent reads and
/// the request counter reports the right answer for the wrong code.
///
/// Depth is the **max** of the legs, not the sum: the open round fetches the footer, the
/// centroid table and the dictionary together, and the legs then issue their ranges in one
/// further round each, concurrently.
pub async fn query<S: BlobStore>(
    store: &S,
    targets: &[Target],
    prefetch: &[Prefetch],
    fusion: Fusion,
    top_k: usize,
) -> Result<Vec<Hit>, QueryError> {
    // ⚠️ A retriever this build cannot RUN is refused before any I/O, and refused once:
    // `Runnable` has no `Trigram` variant, so an arm falling through to `Ok(vec![])` cannot
    // be written.
    //
    // ⚠️ A text leg naming the wrong FIELD is a different question and it moved (M6c.2). It
    // can only be answered by the segment, so it costs the open round — a real regression on
    // an error path, taken deliberately: comparing against `DEFAULT_TEXT_FIELD` was free and
    // was the right answer only while every segment was built over the constant.
    let runnable = prefetch
        .iter()
        .map(Runnable::try_from)
        .collect::<Result<Vec<_>, _>>()?;

    // ⚠️ Every segment and every sidecar in ONE round. `Engine::scan`'s comment already made
    // this argument with a measurement: a loop over segment refs "turns a ten-segment index
    // into a twenty-one-hop query", which is six times the whole latency budget. Width is
    // free; depth is not.
    let opened =
        futures_util::future::try_join_all(targets.iter().map(|t| open(store, t, prefetch)))
            .await?;

    // ⚠️ Global statistics, gathered before any leg runs and costing no round trip: the
    // summaries ride in the term dictionaries the open round already fetched. That is what
    // D-30's two-pass IDF must not cost, and per-segment IDF is wrong exactly when the
    // query's discriminating term is the one whose frequency differs between segments.
    //
    // ⚠️ A segment with no text index contributes nothing, and that is correct rather than an
    // omission: it holds no documents containing the field, so it is not part of BM25's
    // corpus.
    //
    // ⚠️ The other case — postings present, dictionary unreadable — would drop the segment
    // out of `doc_count` and every `df`, scoring the whole query against a corpus one segment
    // too small. It cannot produce a wrong ANSWER, because that segment's own text leg fails
    // on the same missing sidecar and `try_join_all` fails the query with it. A guard here
    // was written first and removed: no test could distinguish it, and the mutation gate said
    // so. `a_missing_term_dictionary_is_an_error_not_a_smaller_corpus` pins the property
    // wherever it is enforced.
    let stats: &Stats = &Stats::merge(opened.iter().filter_map(|o| {
        let raw = o.terms.as_ref()?;
        TextIndex::from_segment(&o.segment, raw.as_ref())
            .ok()
            .map(|idx| idx.summary())
    }));

    // ⚠️ N x R futures, still one round: no leg's ranges depend on another's contents.
    let per_segment =
        futures_util::future::try_join_all(opened.iter().zip(targets).enumerate().flat_map(
            |(i, (o, t))| {
                runnable.iter().enumerate().map(move |(j, r)| async move {
                    leg(store, &t.segment, o, r, i, stats)
                        .await
                        .map(|hits| (j, hits))
                })
            },
        ))
        .await?;

    // ⚠️ Regrouped by RETRIEVER, not by segment, and each retriever's union re-ranked before
    // it is fused. `fuse` reads a leg's position in the vec as its rank, so concatenating
    // segment answers unsorted would hand segment 0's hits ranks 1..n and segment 1's
    // ranks n+1.., ranking a whole segment above another for no reason. Fusing per segment
    // and merging is the other wrong shape: RRF is blind to score magnitude, so the top hit
    // of every segment would earn the same 1/(k+1) however much worse it is.
    let mut legs: Vec<Vec<Hit>> = vec![Vec::new(); runnable.len()];
    for (j, hits) in per_segment {
        if let Some(leg) = legs.get_mut(j) {
            leg.extend(hits);
        }
    }
    for leg in &mut legs {
        // Every retriever in this build ranks higher-is-better; ties break on the pair, the
        // same rule `fuse` itself applies, so the union's order is not a new convention.
        leg.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| (a.segment, a.row).cmp(&(b.segment, b.row)))
        });
    }
    Ok(fuse(&legs, fusion, top_k))
}

/// A leg this build can actually run.
enum Runnable<'a> {
    Text {
        field: &'a str,
        query: &'a str,
        limit: usize,
    },
    Dense {
        field: &'a str,
        query: &'a [f32],
        limit: usize,
        tune: vec_index::Query,
    },
    Sparse {
        field: &'a str,
        query: &'a [(u32, f32)],
        limit: usize,
    },
}

impl<'a> TryFrom<&'a Prefetch> for Runnable<'a> {
    type Error = QueryError;

    fn try_from(p: &'a Prefetch) -> Result<Self, QueryError> {
        match p {
            Prefetch::Dense {
                field,
                query,
                limit,
                tune,
            } => Ok(Self::Dense {
                field,
                query,
                limit: *limit,
                tune: *tune,
            }),
            Prefetch::Sparse {
                field,
                query,
                limit,
            } => Ok(Self::Sparse {
                field,
                query,
                limit: *limit,
            }),
            // ⚠️ The field is CHECKED, not ignored — but the check is in `leg`, against the
            // name the SEGMENT carries. A segment has one text field and a request naming
            // another would otherwise be answered with that field's ranking, with nothing
            // anywhere saying so — D-73's failure at field granularity. Comparing against
            // `DEFAULT_TEXT_FIELD` here was the same check while every segment was built over
            // the constant, and became the failure itself the moment one was not.
            Prefetch::Text {
                field,
                query,
                limit,
            } => Ok(Self::Text {
                field,
                query,
                limit: *limit,
            }),
            Prefetch::Trigram { .. } => Err(QueryError::Unimplemented("trigram")),
        }
    }
}

/// The open round: the footer and every sidecar any leg needs, together.
async fn open<S: BlobStore>(
    store: &S,
    target: &Target,
    prefetch: &[Prefetch],
) -> Result<Opened, QueryError> {
    let (key, centroids) = (&target.segment, &target.centroids);
    let wants_dense = prefetch.iter().any(|p| matches!(p, Prefetch::Dense { .. }));
    let wants_sparse = prefetch
        .iter()
        .any(|p| matches!(p, Prefetch::Sparse { .. }));
    let wants_text = prefetch.iter().any(|p| matches!(p, Prefetch::Text { .. }));
    // ⚠️ Three futures, one round. Every key is derived and none depends on another's
    // contents, so awaiting them in sequence would cost a round trip per modality — which is
    // exactly what `prefetch[]` must not turn into.
    let (segment, cen, dict, terms) = futures_util::future::join4(
        Segment::open(store, key),
        maybe(store, wants_dense.then(|| centroids.clone())),
        maybe(
            store,
            wants_sparse.then(|| pstore_format::sparse::dict_key(key)),
        ),
        maybe(
            store,
            wants_text.then(|| pstore_format::text::dict_key(key)),
        ),
    )
    .await;
    Ok(Opened {
        segment: segment?,
        centroids: cen,
        dictionary: dict,
        terms,
    })
}

/// Fetches an immutable sidecar, or nothing at all when no leg needs it.
///
/// A missing centroid object is how an index below the exact-scan threshold says "scan me
/// exactly" (D-10), so absence is not an error here either.
async fn maybe<S: BlobStore>(store: &S, key: Option<Key>) -> Option<bytes::Bytes> {
    let key = key?;
    store
        .get_immutable(&key, pstore_blob::Class::Pinned)
        .await
        .ok()
}

/// One retriever's ranked answer.
async fn leg<S: BlobStore>(
    store: &S,
    key: &Key,
    opened: &Opened,
    r: &Runnable<'_>,
    segment: usize,
    stats: &Stats,
) -> Result<Vec<Hit>, QueryError> {
    match r {
        Runnable::Text {
            field,
            query,
            limit,
        } => {
            // ⚠️ Refused, never answered from the wrong field. An empty result would be the
            // kinder-looking failure and the worse one: a caller cannot tell it from a term
            // that simply does not occur.
            if !opened.segment.text_fields().iter().any(|f| f == field) {
                return Err(QueryError::Format(FormatError::UnknownField));
            }
            let raw = opened.terms.as_ref().ok_or(FormatError::Corrupt(
                "a text leg over a segment with no term dictionary sidecar",
            ))?;
            let idx = TextIndex::from_segment(&opened.segment, raw.as_ref())?;
            // ⚠️ The statistics are the CALLER's, summed across every segment in the query,
            // not this segment's own summary. That caller is what M5a, M5b and M5c each
            // handed forward by name, and scoring against a per-segment summary is wrong
            // exactly when the query's discriminating term is the one whose frequency
            // differs between segments — which M5c measured.
            let terms = pstore_format::text::analyze(query);
            let hits = idx.search(store, key, &terms, stats, *limit).await?;
            Ok(hits
                .into_iter()
                .map(|(row, score)| Hit {
                    segment,
                    row,
                    score,
                })
                .collect())
        }
        Runnable::Dense {
            field,
            query,
            limit,
            tune,
        } => {
            let idx = VecIndex::from_parts(
                opened.segment.clone(),
                opened.centroids.as_ref().map(AsRef::as_ref),
                query.len(),
            );
            let hits = idx
                .search_field(
                    store,
                    key,
                    field,
                    query,
                    vec_index::Query { k: *limit, ..*tune },
                )
                .await?;
            Ok(hits
                .into_iter()
                .map(|(row, score)| Hit {
                    segment,
                    row,
                    score,
                })
                .collect())
        }
        Runnable::Sparse {
            field,
            query,
            limit,
        } => {
            let raw = opened.dictionary.as_ref().ok_or(FormatError::Corrupt(
                "a sparse leg over a segment with no dictionary sidecar",
            ))?;
            let idx = SparseIndex::from_segment(&opened.segment, field, raw.as_ref())?;
            let hits = idx.search(store, key, query, *limit).await?;
            Ok(hits
                .into_iter()
                .map(|(row, score)| Hit {
                    segment,
                    row,
                    score,
                })
                .collect())
        }
    }
}
