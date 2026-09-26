//! Running the legs — concurrently, over one open segment.

use crate::filter::{Mask, Predicate};
use crate::fuse::{Fusion, Hit, fuse};
use pstore_blob::{BlobStore, Key};
use pstore_format::{Document, FormatError, Segment};
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
    /// Its delete vector, when it has one (M9c). Fetched in the open round.
    pub deleted: Option<Key>,
    /// Whether the query's shadowed ids hide rows here. `false` for the fresh segment, whose
    /// rows ARE the newest versions.
    pub shadowed: bool,
}

/// Which sidecars a query needs, derived from the legs.
struct Opened {
    segment: Segment,
    centroids: Option<bytes::Bytes>,
    dictionary: Option<bytes::Bytes>,
    terms: Option<bytes::Bytes>,
    /// Rows its delete vector excludes; empty when it has none.
    deleted: std::collections::HashSet<usize>,
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
    Ok(run(
        store,
        targets,
        prefetch,
        None,
        &std::collections::HashSet::new(),
        fusion,
        top_k,
    )
    .await?
    .0)
}

/// The ranking **and the segments it was computed over**, still open.
///
/// ⚠️ Separated from [`query`] for one reason: an id lives in a block of a segment this
/// function has already opened, and a caller that re-opens it pays a round trip per segment
/// for something already in hand.
async fn run<S: BlobStore>(
    store: &S,
    targets: &[Target],
    prefetch: &[Prefetch],
    filter: Option<&Predicate>,
    shadow: &std::collections::HashSet<String>,
    fusion: Fusion,
    top_k: usize,
) -> Result<(Vec<Hit>, Vec<Opened>, Known, Dense), QueryError> {
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
    //
    // ⚠️ **With a filter, every leg is exhaustive and the mask rides the same round** (M9b).
    // A leg that stopped at its `limit` and was filtered afterwards would return fewer than
    // `limit` whenever the predicate is selective, silently -- so each leg returns every
    // candidate (the dense leg probing every list: `p` costs bytes, not depth), the mask
    // removes what the predicate does not admit, and only then is the limit applied. The
    // mask's blocks are addressable the moment the segment is open, exactly as the legs'
    // ranges are, so fetching it alongside them adds no round trip.
    //
    // ⚠️ **Superseded rows are excluded the same way** (M9c): a row in a segment's delete
    // vector, or -- in a shadowed segment -- one whose id has a newer unfolded operation. Without
    // a filter the legs need not be exhaustive: at most `deleted + shadowed` of a segment's rows
    // can be excluded, so a leg asked for that many more still has `limit` left after them.
    let legs_fut =
        futures_util::future::try_join_all(opened.iter().zip(targets).enumerate().flat_map(
            |(i, (o, t))| {
                runnable.iter().enumerate().map(move |(j, r)| async move {
                    let hidden = o.deleted.len() + if t.shadowed { shadow.len() } else { 0 };
                    let r = match filter {
                        Some(_) => r.exhaustive(o.segment.index_row_count()),
                        None => r.widened(hidden, o.segment.row_count()),
                    };
                    leg(store, &t.segment, o, &r, i, stats)
                        .await
                        .map(|hits| (i, j, hits))
                })
            },
        ));
    let masks_fut =
        futures_util::future::try_join_all(opened.iter().zip(targets).map(|(o, t)| async move {
            match filter {
                Some(f) => mask(store, &t.segment, &o.segment, f).await.map(Some),
                None => Ok(None),
            }
        }));
    let (per_segment, masks) = futures_util::future::try_join(legs_fut, masks_fut).await?;
    let candidates: Vec<(usize, usize, Vec<Hit>)> = per_segment
        .into_iter()
        .map(|(i, j, hits)| {
            let admitted = masks.get(i).and_then(Option::as_ref);
            let deleted = opened.get(i).map(|o| &o.deleted);
            // ⚠️ **Cut to what can survive the shadow**, before its documents are fetched
            // (review of M9c.2): at most `|shadow|` of a segment's candidates are shadowed, so
            // the rest of the widened list can never reach the answer -- and fetching it read a
            // block per candidate, a number that grows with the deleted count.
            let room = runnable.get(j).map_or(0, Runnable::limit)
                + if targets.get(i).is_some_and(|t| t.shadowed) {
                    shadow.len()
                } else {
                    0
                };
            let kept = hits
                .into_iter()
                .filter(|h| admitted.is_none_or(|m| m.contains(&h.row)))
                .filter(|h| deleted.is_none_or(|d| !d.contains(&h.row)))
                .take(room)
                .collect();
            (i, j, kept)
        })
        .collect();

    // ⚠️ **Shadowing is checked on the CANDIDATES' ids** (M9c.2, spec review): reading every
    // block of every segment to find the shadowed rows would read the whole index on every
    // query of a process with anything unfolded. The candidates' documents are fetched in one
    // fan-out round, and it is the round that resolves the answer's ids anyway: `known` carries
    // them there, so the depth is unchanged.
    // A non-empty shadow comes with the index's segments as shadowed targets, or with none.
    let known = if !shadow.is_empty() {
        let wanted: Vec<(usize, usize)> = candidates
            .iter()
            .flat_map(|(i, _, hits)| hits.iter().map(move |h| (*i, h.row)))
            .collect();
        fetch_rows(store, targets, &opened, &wanted).await?
    } else {
        std::collections::BTreeMap::new()
    };
    let per_segment = candidates.into_iter().map(|(i, j, hits)| {
        let shadowed = targets.get(i).is_some_and(|t| t.shadowed);
        let limit = runnable.get(j).map_or(0, Runnable::limit);
        let kept: Vec<Hit> = hits
            .into_iter()
            .filter(|h| {
                !shadowed
                    || known
                        .get(&(i, h.row))
                        .is_none_or(|d| !shadow.contains(&d.id))
            })
            .take(limit)
            .collect();
        (j, kept)
    });

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
    // ⚠️ **The dense leg's own score, kept before fusion replaces it** (M9d): a fused score is
    // `Σ 1/(k + rank)` whatever the legs were, so `$dist` has no other source.
    let dense: Dense = runnable
        .iter()
        .position(|r| matches!(r, Runnable::Dense { .. }))
        .and_then(|j| legs.get(j))
        .map(|leg| leg.iter().map(|h| ((h.segment, h.row), h.score)).collect())
        .unwrap_or_default();
    Ok((fuse(&legs, fusion, top_k), opened, known, dense))
}

/// The first dense leg's score of each row it ranked, by `(segment, row)` (M9d).
type Dense = std::collections::BTreeMap<(usize, usize), f32>;

/// The ranking, **and the id of every hit**, in one more round than the ranking alone.
///
/// ⚠️ **Why this is here and not in the caller.** Resolving a hit needs the block that holds
/// its row, and only this function still has the opened segments: a caller doing it would
/// re-open each one — `Segment::open` then `ids_at`, two data-dependent rounds **per
/// segment**, serially. Measured that way at eight segments: **19 sequential round trips**
/// for one query, against a budget of three. Here it is a single fan-out round whatever the
/// segment count, because the opens already happened and no block's address depends on
/// another's contents.
///
/// ⚠️ **It is still a fourth round, and D-34 allows three.** The ranking is three; carrying
/// the ids costs one more, because a payload's address cannot be known before the ranking
/// exists. The way to three is a format change — ids fetched alongside the vectors the leg
/// already reads — and that is a decision about what a segment stores, recorded in the
/// backlog rather than made here.
///
/// ⚠️ **And each hit's attributes, from the same round** (M9a). The blocks that resolve the
/// ids carry the attributes, so they are kept rather than decoded and dropped; each document
/// has an empty `vectors`. This replaced `query_ids`, whose last caller it was.
///
/// ⚠️ **Only with documents `filter` admits**, when there is one (M9b): see `run`.
///
/// And each hit's score in the first dense leg, if that leg ranked it (M9d): the fused score
/// is a rank sum, and `$dist` is computed from this.
///
/// # Errors
/// As [`query`], plus a block that cannot be read or decoded.
pub async fn query_rows_filtered<S: BlobStore>(
    store: &S,
    targets: &[Target],
    prefetch: &[Prefetch],
    filter: Option<&Predicate>,
    shadow: &std::collections::HashSet<String>,
    fusion: Fusion,
    top_k: usize,
) -> Result<Vec<(Hit, Option<Document>, Option<f32>)>, QueryError> {
    let (hits, opened, known, dense) =
        run(store, targets, prefetch, filter, shadow, fusion, top_k).await?;
    Ok(resolve_rows(store, targets, &opened, &hits, known)
        .await?
        .into_iter()
        .map(|(h, d)| {
            let score = dense.get(&(h.segment, h.row)).copied();
            (h, d, score)
        })
        .collect())
}

/// The data rows of one segment `filter` admits, from its blocks, zone-map pruned.
async fn mask<S: BlobStore>(
    store: &S,
    key: &Key,
    segment: &Segment,
    filter: &Predicate,
) -> Result<Mask, QueryError> {
    Ok(segment
        .rows_where(store, key, |zones| filter.could_admit(zones))
        .await?
        .into_iter()
        .filter(|(_, d)| filter.admits(&d.id, &d.attrs))
        .map(|(row, _)| row)
        .collect())
}

/// Documents already fetched this query, by `(segment, row)`.
type Known = std::collections::BTreeMap<(usize, usize), Document>;

/// The documents at `(segment, row)` pairs: one fan-out round over the segments holding them.
async fn fetch_rows<S: BlobStore>(
    store: &S,
    targets: &[Target],
    opened: &[Opened],
    wanted: &[(usize, usize)],
) -> Result<Known, QueryError> {
    let mut rows_per_segment: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (segment, row) in wanted {
        rows_per_segment.entry(*segment).or_default().push(*row);
    }
    // ⚠️ One round: every segment's blocks are fetched together. Serially this is where the
    // depth went from 3 to 3 + 2N.
    let resolved = futures_util::future::try_join_all(rows_per_segment.iter().map(
        |(segment, rows)| async move {
            // ⚠️ The segment is **already open** — `run` opened every one of them in its
            // first round. Re-opening here is what made depth `3 + 2N`.
            let (Some(t), Some(o)) = (targets.get(*segment), opened.get(*segment)) else {
                return Ok::<_, QueryError>((*segment, Vec::new()));
            };
            let docs = o.segment.rows_at(store, &t.segment, rows).await?;
            Ok((*segment, rows.iter().copied().zip(docs).collect::<Vec<_>>()))
        },
    ))
    .await?;
    let mut out = Known::new();
    for (segment, pairs) in resolved {
        for (row, doc) in pairs {
            if let Some(doc) = doc {
                out.insert((segment, row), doc);
            }
        }
    }
    Ok(out)
}

/// The rows for `hits`, one fan-out round over the segments that carry them.
async fn resolve_rows<S: BlobStore>(
    store: &S,
    targets: &[Target],
    opened: &[Opened],
    hits: &[Hit],
    mut known: Known,
) -> Result<Vec<(Hit, Option<Document>)>, QueryError> {
    // Only what the shadow check did not already fetch (M9c.2): when it ran, this is nothing.
    let missing: Vec<(usize, usize)> = hits
        .iter()
        .map(|h| (h.segment, h.row))
        .filter(|k| !known.contains_key(k))
        .collect();
    if !missing.is_empty() {
        known.extend(fetch_rows(store, targets, opened, &missing).await?);
    }
    Ok(hits
        .iter()
        .map(|h| (*h, known.get(&(h.segment, h.row)).cloned()))
        .collect())
}

/// A leg this build can actually run.
#[derive(Clone, Copy)]
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

impl Runnable<'_> {
    /// Rows this leg contributes at most.
    fn limit(&self) -> usize {
        match self {
            Self::Text { limit, .. } | Self::Dense { limit, .. } | Self::Sparse { limit, .. } => {
                *limit
            }
        }
    }

    /// The same leg asking for `extra` more rows than its limit: room for rows the query will
    /// exclude as superseded (M9c), so `limit` survive them. A clustered dense leg also probes
    /// `rows / live` times as many lists (spec review): excluded rows are candidates it spends
    /// probes on, and more `k` from the same lists cannot replace them.
    fn widened(self, extra: usize, rows: usize) -> Self {
        match self {
            Self::Text {
                field,
                query,
                limit,
            } => Self::Text {
                field,
                query,
                limit: limit + extra,
            },
            Self::Sparse {
                field,
                query,
                limit,
            } => Self::Sparse {
                field,
                query,
                limit: limit + extra,
            },
            Self::Dense {
                field,
                query,
                limit,
                tune,
            } => {
                let live = rows.saturating_sub(extra).max(1);
                Self::Dense {
                    field,
                    query,
                    limit: limit + extra,
                    tune: vec_index::Query {
                        p: tune.p.saturating_mul(rows.max(1)).div_ceil(live),
                        ..tune
                    },
                }
            }
        }
    }

    /// The same leg returning **every** candidate of a segment of `rows` rows: a filter
    /// masks it before its limit applies (M9b). The dense leg probes every list and keeps
    /// every candidate through the ladder.
    fn exhaustive(self, rows: usize) -> Self {
        let rows = rows.max(1);
        match self {
            Self::Text { field, query, .. } => Self::Text {
                field,
                query,
                limit: rows,
            },
            Self::Sparse { field, query, .. } => Self::Sparse {
                field,
                query,
                limit: rows,
            },
            Self::Dense {
                field, query, tune, ..
            } => Self::Dense {
                field,
                query,
                limit: rows,
                tune: vec_index::Query {
                    p: usize::MAX,
                    ..tune
                },
            },
        }
    }
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
    // ⚠️ The delete vector rides the same round (M9c): its key is derived, like the others.
    // Unlike them, a named vector that cannot be read is an ERROR -- answering without it would
    // return rows a newer write superseded, which is a wrong answer rather than a slow one.
    let dv = async {
        match &target.deleted {
            Some(k) => store
                .get_immutable(k, pstore_blob::Class::Pinned)
                .await
                .map(|raw| crate::deletes::decode(&raw))
                .map_err(|e| QueryError::Format(FormatError::from(e))),
            None => Ok(std::collections::HashSet::new()),
        }
    };
    let (segment, cen, dict, terms, deleted) = futures_util::future::join5(
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
        dv,
    )
    .await;
    Ok(Opened {
        segment: segment?,
        centroids: cen,
        dictionary: dict,
        terms,
        deleted: deleted?,
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
            // ⚠️ **The dimension comes from the SEGMENT, never from the query.** Passing
            // `query.len()` here let the index take its idea of the dimension from the
            // caller, so a two-dimensional query over a four-dimensional segment came back
            // with scored, ranked, plausible rows and a `200`. The exact path has always
            // refused it (`Segment::search` compares against the row it read); the
            // approximate one did not, and the two disagreeing is worse than either.
            // Found through the API in M7c.
            let dim = opened
                .segment
                .field_layout(field)
                .map_or(query.len(), |f| f.dims as usize);
            let idx = VecIndex::from_parts(
                opened.segment.clone(),
                opened.centroids.as_ref().map(AsRef::as_ref),
                dim,
            );
            if dim != query.len() {
                return Err(pstore_format::FormatError::DimensionMismatch {
                    expected: dim,
                    got: query.len(),
                }
                .into());
            }
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
