//! Running the legs — concurrently, over one open segment.

use crate::filter::{Mask, Predicate};
use crate::fuse::{Fusion, Hit, Weights, fuse};
use pstore_blob::{BlobStore, Key};
use pstore_format::text::FullText;
use pstore_format::{Document, FormatError, Segment};
use pstore_index::sparse::SparseIndex;
use pstore_index::text::{Stats, TextIndex};
use pstore_index::vec_index::{self, VecIndex};
use std::sync::Arc;

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
    /// Its length in bytes, when HEAD recorded it (M45): the open reads an absolute range
    /// rather than a suffix, which a backend without suffix ranges (Azure, C-14) can serve.
    pub segment_len: Option<u64>,
    /// Its centroid table, or `None` when the caller knows it has none (M27), and the open
    /// round asks for nothing. Absent in the store is not an error either: D-10 reads both as
    /// "scan me exactly".
    pub centroids: Option<Key>,
    /// Its delete vector, when it has one (M9c). Fetched in the open round.
    pub deleted: Option<Key>,
    /// Whether it may have a sparse dictionary, and a term dictionary (M32): `false` when the
    /// caller knows it has none, and the open round asks for nothing.
    pub sparse_dict: bool,
    /// See `sparse_dict`.
    pub text_dict: bool,
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
pub async fn query<S: BlobStore + Clone>(
    store: &S,
    targets: &[Target],
    prefetch: &[Prefetch],
    fusion: Fusion,
    top_k: usize,
) -> Result<Vec<Hit>, QueryError> {
    query_with(
        store,
        targets,
        prefetch,
        &FullText::default(),
        fusion,
        top_k,
    )
    .await
}

/// [`query`], analyzing and scoring text legs by an index's full-text schema (M14).
///
/// # Errors
/// As [`query`].
pub async fn query_with<S: BlobStore + Clone>(
    store: &S,
    targets: &[Target],
    prefetch: &[Prefetch],
    text: &FullText,
    fusion: Fusion,
    top_k: usize,
) -> Result<Vec<Hit>, QueryError> {
    Ok(run(
        store,
        targets,
        prefetch,
        text,
        None,
        &std::collections::HashSet::new(),
        fusion,
        top_k,
        Vec::new(),
    )
    .await?
    .0)
}

/// Each `(segment, leg)` pair's hits from one share of a query, numbered by the coordinator's
/// segment ordinals and leg positions (M54).
pub type PartHits = Vec<(usize, usize, Vec<Hit>)>;

/// Some of a query's segments, whose legs another server runs (M54, M55).
///
/// ⚠️ **An error is never the query's.** `run` runs a share that failed itself, over the
/// same segments: a peer that is down, slow or wrong costs rounds, never an answer.
pub struct Elsewhere<'a> {
    /// The segment ordinals, into the query's `targets`, that it runs.
    pub targets: Vec<usize>,
    /// How.
    pub share: Share<'a>,
}

/// One share's exchange with its server.
pub enum Share<'a> {
    /// M54: the vector legs only, in one exchange. The coordinator runs the text legs.
    Hits(futures_util::future::BoxFuture<'a, Result<PartHits, String>>),
    /// M55: every leg, in two exchanges -- the share's BM25 statistics for the query's terms,
    /// then its hits scored against the sum over every segment, which only the coordinator can
    /// add up.
    Phased {
        /// Phase 1: the share's statistics.
        stats: futures_util::future::BoxFuture<'a, Result<Stats, String>>,
        /// Phase 2: its hits, given the global sum.
        scan: Box<
            dyn FnOnce(Stats) -> futures_util::future::BoxFuture<'a, Result<PartHits, String>>
                + Send
                + 'a,
        >,
    },
}

/// Whether a leg may run on another server in one exchange (M54): dense and sparse score
/// each row by itself, so their hits over two segments compare wherever they were computed.
/// A text leg scores against statistics summed over every segment, so it needs two (M55).
#[must_use]
pub fn splittable(p: &Prefetch) -> bool {
    matches!(p, Prefetch::Dense { .. } | Prefetch::Sparse { .. })
}

/// One share's segments, opened for its legs and held between a split query's two phases
/// (M55). Owned: it outlives the request that opened it.
pub struct OpenPart {
    targets: Vec<(usize, Target)>,
    legs: Vec<(usize, Prefetch)>,
    opened: Vec<Opened>,
}

impl OpenPart {
    /// Roughly what holding it costs: its sidecars and delete vectors, and 16 KiB a segment
    /// for the footer it keeps decoded. An estimate for a budget, never a measurement.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.opened
            .iter()
            .map(|o| {
                held_bytes(
                    [&o.centroids, &o.dictionary, &o.terms]
                        .iter()
                        .map(|b| b.as_ref().map_or(0, bytes::Bytes::len))
                        .sum::<usize>(),
                    o.deleted.len(),
                )
            })
            .sum()
    }
}

/// One held segment's estimate: 16 KiB for its decoded footer, its sidecars' bytes, and 16
/// bytes a deleted row.
fn held_bytes(sidecars: usize, deleted: usize) -> usize {
    16 * 1024 + sidecars + 16 * deleted
}

#[cfg(test)]
mod held_tests {
    #[test]
    fn a_held_segment_is_priced_by_what_it_keeps() {
        assert_eq!(super::held_bytes(0, 0), 16_384);
        assert_eq!(super::held_bytes(1_000, 0), 17_384);
        assert_eq!(super::held_bytes(1_000, 10), 17_544);
    }
}

/// Phase 1 of a share (M55): `legs` over `targets` opened, and their BM25 statistics for
/// `terms`.
///
/// ⚠️ **Whole or nothing** (spec review): a segment whose footer has a text field and which
/// HEAD says has a term dictionary must yield a summary. The coordinator's own path drops an
/// unreadable dictionary from the sum and relies on that segment's text leg failing the query;
/// a share's statistics travel apart from its legs, so here the share fails instead, and a sum
/// one segment short is never returned.
///
/// # Errors
/// As [`query`], and a term dictionary that should be there and is not.
pub async fn open_part<S: BlobStore>(
    store: &S,
    targets: &[(usize, Target)],
    legs: &[(usize, Prefetch)],
    terms: &[String],
) -> Result<(OpenPart, Stats), QueryError> {
    let prefetch: Vec<Prefetch> = legs.iter().map(|(_, p)| p.clone()).collect();
    let opened =
        futures_util::future::try_join_all(targets.iter().map(|(_, t)| open(store, t, &prefetch)))
            .await?;
    let wants_text = legs.iter().any(|(_, p)| matches!(p, Prefetch::Text { .. }));
    let mut parts = Vec::new();
    for ((_, t), o) in targets.iter().zip(&opened) {
        if !wants_text || !t.text_dict || o.segment.text_fields().is_empty() {
            continue;
        }
        let raw = o.terms.as_ref().ok_or(FormatError::Corrupt(
            "a share's segment with text and no term dictionary",
        ))?;
        parts.push(TextIndex::from_segment(&o.segment, raw.as_ref())?.summary());
    }
    let stats = only(Stats::merge(parts), terms);
    Ok((
        OpenPart {
            targets: targets.to_vec(),
            legs: legs.to_vec(),
            opened,
        },
        stats,
    ))
}

/// `stats` with `df` kept for `terms` only (M55): BM25 reads no other term's, so a share
/// sends the query's few rather than its whole vocabulary.
#[must_use]
pub fn only(mut stats: Stats, terms: &[String]) -> Stats {
    stats.df.retain(|t, _| terms.contains(t));
    stats
}

/// A `sum` query's cut for one share (M58): every leg of the share run whole, fused by
/// `weights` exactly as the coordinator fuses, and only the share's top `keep` rows kept --
/// every leg's hit for each. `keep` is the query's `top_k` plus its shadow's size, what
/// `summed` resolves before dropping the shadowed.
///
/// ⚠️ Exact because a row lives in one segment, and every leg of a segment runs on one
/// server: its sum is the coordinator's to the bit, the share's order is the global order
/// restricted to the share, and so any row of the global top `keep` is in its share's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SumCut {
    /// The query's weights, by global leg number.
    pub weights: Weights,
    /// How many rows the share keeps.
    pub keep: usize,
}

/// `hits` cut to the share's top `cut.keep` rows by weighted sum, every leg's hit for each.
fn cut_sum(hits: PartHits, cut: SumCut) -> PartHits {
    // Never wider than a query may be (code review): a leg's number sizes this.
    let width = hits
        .iter()
        .map(|(_, j, _)| j.saturating_add(1))
        .max()
        .unwrap_or(0)
        .min(crate::MAX_LEGS);
    let mut legs: Vec<Vec<Hit>> = vec![Vec::new(); width];
    for (_, j, h) in &hits {
        if let Some(leg) = legs.get_mut(*j) {
            leg.extend(h.iter().copied());
        }
    }
    let kept: std::collections::HashSet<(usize, usize)> = fuse(
        &legs,
        Fusion::Sum {
            weights: cut.weights,
        },
        cut.keep,
    )
    .iter()
    .map(|h| (h.segment, h.row))
    .collect();
    hits.into_iter()
        .map(|(i, j, h)| {
            (
                i,
                j,
                h.into_iter()
                    .filter(|x| kept.contains(&(x.segment, x.row)))
                    .collect(),
            )
        })
        .collect()
}

/// Phase 2 of a share (M55): every leg over the held segments, scored with `stats`, masked by
/// `filter`, rid of deleted rows and cut to what a shadow of `shadow` ids can leave -- exactly
/// as [`run`] does over its own segments, because it is the same function.
///
/// # Errors
/// As [`query`].
#[allow(
    clippy::too_many_arguments,
    reason = "a share's parts, each needed by a leg"
)]
pub async fn scan_part<S: BlobStore + Clone>(
    store: &S,
    part: &OpenPart,
    filter: Option<&Predicate>,
    shadow: usize,
    stats: &Stats,
    text: &FullText,
    sum: Option<SumCut>,
) -> Result<PartHits, QueryError> {
    let runnable = part
        .legs
        .iter()
        .map(|(j, p)| Runnable::try_from(p).map(|r| (*j, r)))
        .collect::<Result<Vec<_>, _>>()?;
    let items: Vec<(usize, &Target, &Opened)> = part
        .targets
        .iter()
        .zip(&part.opened)
        .map(|((i, t), o)| (*i, t, o))
        .collect();
    // M58: under `sum` the legs run whole, as `run` runs its own, and the share is cut by sum.
    let hits = candidates(
        store,
        &items,
        &runnable,
        |_, _| true,
        filter,
        shadow,
        sum.is_some(),
        &Arc::new(stats.clone()),
        text,
    )
    .await?;
    Ok(match sum {
        Some(cut) => cut_sum(hits, cut),
        None => hits,
    })
}

/// One server's share of a query in one call (M54): [`open_part`] then [`scan_part`], scored
/// with `stats` -- the global sum when a text leg is among `legs` (M55), and anything when
/// none is.
///
/// # Errors
/// As [`query`].
#[allow(
    clippy::too_many_arguments,
    reason = "a share's parts, each needed by a leg"
)]
pub async fn part<S: BlobStore + Clone>(
    store: &S,
    targets: &[(usize, Target)],
    legs: &[(usize, Prefetch)],
    filter: Option<&Predicate>,
    shadow: usize,
    stats: &Stats,
    text: &FullText,
    sum: Option<SumCut>,
) -> Result<PartHits, QueryError> {
    let prefetch: Vec<Prefetch> = legs.iter().map(|(_, p)| p.clone()).collect();
    let opened =
        futures_util::future::try_join_all(targets.iter().map(|(_, t)| open(store, t, &prefetch)))
            .await?;
    let held = OpenPart {
        targets: targets.to_vec(),
        legs: legs.to_vec(),
        opened,
    };
    scan_part(store, &held, filter, shadow, stats, text, sum).await
}

/// Every running `(segment, leg)` pair's candidates: each leg widened by what the segment may
/// exclude, or exhaustive under a filter or `Sum`; masked; rid of deleted rows; and cut to what
/// can survive the shadow. `runs` says which pairs run here (M54).
#[allow(
    clippy::too_many_arguments,
    reason = "the query's parts, each needed by a leg"
)]
async fn candidates<S: BlobStore + Clone>(
    store: &S,
    items: &[(usize, &Target, &Opened)],
    legs: &[(usize, Runnable<'_>)],
    runs: impl Fn(usize, usize) -> bool,
    filter: Option<&Predicate>,
    shadow: usize,
    whole: bool,
    stats: &Arc<Stats>,
    text: &FullText,
) -> Result<PartHits, QueryError> {
    let runs = &runs;
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
    // ⚠️ **Under `Sum`, every leg is whole** (M9g.2, spec review): a leg cut at its limit drops
    // a row's contribution, so the best sum can be a row no leg ranks first -- and whether it
    // survives would hang on how the index is cut into segments. So the leg is exhaustive and
    // no cut is re-applied; the fused ranking is what is cut, below.
    //
    // ⚠️ **Each pair on a task of its own** (M59). Joined in one task, every segment's scoring
    // -- the index's, right after its read -- held the only thread the query had: 48 segments
    // scored one after another while the runtime's other workers sat idle, about 1 ms each at
    // 2,000 rows. A task owns what it reads: the store's handle, the segment's footer and
    // sidecars, the leg rebuilt owned, and the query's statistics, shared -- never copied a
    // task: the coordinator's hold its whole vocabulary.
    let mut tasks = Tasks(Vec::new());
    for &(i, t, o) in items {
        // One copy a segment, shared by its legs (code review), without the delete vector: a
        // leg never reads it, and deleted rows are dropped below, from `items`.
        let shared = Arc::new(Opened {
            segment: o.segment.clone(),
            centroids: o.centroids.clone(),
            dictionary: o.dictionary.clone(),
            terms: o.terms.clone(),
            deleted: std::collections::HashSet::new(),
        });
        for &(j, r) in legs.iter().filter(|(j, _)| runs(i, *j)) {
            let hidden = o.deleted.len() + if t.shadowed { shadow } else { 0 };
            let r = if filter.is_some() || whole {
                r.exhaustive(o.segment.index_row_count())
            } else {
                r.widened(hidden, o.segment.row_count())
            };
            let owned = r.owned();
            let (store, key, stats, text) =
                (store.clone(), t.segment.clone(), Arc::clone(stats), *text);
            let opened = Arc::clone(&shared);
            tasks.0.push(tokio::spawn(async move {
                // Cannot fail: `legs` holds only legs this build runs, refused before any I/O.
                let r = Runnable::try_from(&owned)?;
                leg(&store, &key, &opened, &r, i, &stats, &text)
                    .await
                    .map(|hits| (i, j, hits))
            }));
        }
    }
    let legs_fut = tasks.join();
    // ⚠️ A mask only for a segment some leg runs over here (M54): a segment whose vector legs
    // run elsewhere is masked there.
    let masks_fut = futures_util::future::try_join_all(items.iter().map(|&(i, t, o)| async move {
        match filter {
            Some(f) if legs.iter().any(|(j, _)| runs(i, *j)) => {
                mask(store, &t.segment, &o.segment, f)
                    .await
                    .map(|m| (i, Some(m)))
            }
            _ => Ok((i, None)),
        }
    }));
    let (per_segment, masks) = futures_util::future::try_join(legs_fut, masks_fut).await?;
    let masks: std::collections::BTreeMap<usize, Mask> = masks
        .into_iter()
        .filter_map(|(i, m)| m.map(|m| (i, m)))
        .collect();
    let by_ordinal: std::collections::BTreeMap<usize, (&Target, &Opened)> =
        items.iter().map(|&(i, t, o)| (i, (t, o))).collect();
    Ok(per_segment
        .into_iter()
        .map(|(i, j, hits)| {
            let admitted = masks.get(&i);
            let deleted = by_ordinal.get(&i).map(|(_, o)| &o.deleted);
            // ⚠️ **Cut to what can survive the shadow**, before its documents are fetched
            // (review of M9c.2): at most `|shadow|` of a segment's candidates are shadowed, so
            // the rest of the widened list can never reach the answer -- and fetching it read a
            // block per candidate, a number that grows with the deleted count.
            let room = if whole {
                usize::MAX
            } else {
                legs.iter()
                    .find(|(k, _)| *k == j)
                    .map_or(0, |(_, r)| r.limit())
                    + if by_ordinal.get(&i).is_some_and(|(t, _)| t.shadowed) {
                        shadow
                    } else {
                        0
                    }
            };
            let kept = hits
                .into_iter()
                .filter(|h| filter.is_none() || admitted.is_some_and(|m| m.contains(&h.row)))
                .filter(|h| deleted.is_none_or(|d| !d.contains(&h.row)))
                .take(room)
                .collect();
            (i, j, kept)
        })
        .collect())
}

/// The ranking **and the segments it was computed over**, still open.
///
/// ⚠️ Separated from [`query`] for one reason: an id lives in a block of a segment this
/// function has already opened, and a caller that re-opens it pays a round trip per segment
/// for something already in hand.
#[allow(
    clippy::too_many_arguments,
    reason = "the query's parts, each needed by a leg"
)]
async fn run<S: BlobStore + Clone>(
    store: &S,
    targets: &[Target],
    prefetch: &[Prefetch],
    text: &FullText,
    filter: Option<&Predicate>,
    shadow: &std::collections::HashSet<String>,
    fusion: Fusion,
    top_k: usize,
    elsewhere: Vec<Elsewhere<'_>>,
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
    //
    // ⚠️ **A segment whose vector legs run elsewhere is opened for its text legs only** (M54):
    // its footer, delete vector and term dictionary, which the text legs, the shadow check and
    // the row fetch need -- never the centroid table or sparse dictionary its peer reads.
    // M54: a segment in a one-exchange share has its vector legs run elsewhere, and is opened
    // here for its text legs. M55: a segment in a phased share has every leg run elsewhere, and
    // is opened here for its footer and delete vector only -- what the shadow check and the row
    // fetch need.
    let mut vector_remote = std::collections::BTreeSet::new();
    let mut all_remote = std::collections::BTreeSet::new();
    let mut hit_shares: Vec<(Vec<usize>, futures_util::future::BoxFuture<'_, _>)> = Vec::new();
    let mut phased: Vec<(Vec<usize>, futures_util::future::BoxFuture<'_, _>, _)> = Vec::new();
    for e in elsewhere {
        match e.share {
            Share::Hits(f) => {
                vector_remote.extend(e.targets.iter().copied());
                hit_shares.push((e.targets, f));
            }
            Share::Phased { stats, scan } => {
                all_remote.extend(e.targets.iter().copied());
                phased.push((e.targets, stats, scan));
            }
        }
    }
    let text_legs: Vec<Prefetch> = prefetch
        .iter()
        .filter(|p| !splittable(p))
        .cloned()
        .collect();
    let whole = matches!(fusion, Fusion::Sum { .. });
    let legs: Vec<(usize, Runnable<'_>)> = runnable.iter().copied().enumerate().collect();
    let all_legs: Vec<(usize, Prefetch)> = prefetch.iter().cloned().enumerate().collect();
    let vector_legs: Vec<(usize, Prefetch)> = all_legs
        .iter()
        .filter(|(_, p)| splittable(p))
        .cloned()
        .collect();
    let opening = futures_util::future::try_join_all(targets.iter().enumerate().map(|(i, t)| {
        let wanted: &[Prefetch] = if all_remote.contains(&i) {
            &[]
        } else if vector_remote.contains(&i) {
            text_legs.as_slice()
        } else {
            prefetch
        };
        open(store, t, wanted)
    }));
    let (mut phase_ones, mut scans): (Vec<_>, Vec<_>) = (Vec::new(), Vec::new());
    let mut phased_targets = Vec::new();
    for (t, stats, scan) in phased {
        phased_targets.push(t);
        phase_ones.push(stats);
        scans.push(scan);
    }
    let (hit_targets, hit_futures): (Vec<Vec<usize>>, Vec<_>) = hit_shares.into_iter().unzip();
    // ⚠️ **Beside the open round, not after it** (M54): a peer's share is two rounds on its
    // own store -- or, phased (M55), one round before the statistics and one after -- the same
    // rounds the coordinator spends here, so the slowest path stays HEAD, open, legs, rows.
    let hits_started = futures_util::future::join_all(hit_futures);
    let main = async {
        let (opened, firsts) =
            futures_util::future::join(opening, futures_util::future::join_all(phase_ones)).await;
        let opened = opened?;

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
        let mut stats = Stats::merge(opened.iter().filter_map(|o| {
            let raw = o.terms.as_ref()?;
            TextIndex::from_segment(&o.segment, raw.as_ref())
                .ok()
                .map(|idx| idx.summary())
        }));
        // M55: each phased share's statistics, or -- for a share whose phase 1 failed -- its
        // segments opened here, one more round, so the sum is never a share short.
        let mut scan_here: Vec<(usize, Target)> = Vec::new();
        let mut to_scan = Vec::new();
        let mut summed_parts = Vec::new();
        for ((share, first), scan) in phased_targets.iter().zip(firsts).zip(scans) {
            match first {
                Ok(st) => {
                    summed_parts.push(st);
                    to_scan.push((share, scan));
                }
                Err(_) => scan_here.extend(
                    share
                        .iter()
                        .filter_map(|i| targets.get(*i).map(|t| (*i, t.clone()))),
                ),
            }
        }
        let opened_here = futures_util::future::try_join_all(
            scan_here.iter().map(|(_, t)| open(store, t, prefetch)),
        )
        .await?;
        stats = Stats::merge(std::iter::once(stats).chain(summed_parts).chain(
            opened_here.iter().filter_map(|o| {
                let raw = o.terms.as_ref()?;
                TextIndex::from_segment(&o.segment, raw.as_ref())
                    .ok()
                    .map(|idx| idx.summary())
            }),
        ));
        // Shared by every leg task (M59), and built once.
        let stats = &Arc::new(stats);

        let in_scan_here: std::collections::BTreeSet<usize> =
            scan_here.iter().map(|(i, _)| *i).collect();
        let here = |i: usize, j: usize| {
            !all_remote.contains(&i)
                && (!vector_remote.contains(&i) || prefetch.get(j).is_some_and(|p| !splittable(p)))
        };
        let items: Vec<(usize, &Target, &Opened)> = targets
            .iter()
            .zip(&opened)
            .enumerate()
            .map(|(i, (t, o))| (i, t, o))
            .filter(|(i, _, _)| !in_scan_here.contains(i))
            .chain(
                scan_here
                    .iter()
                    .zip(&opened_here)
                    .map(|((i, t), o)| (*i, t, o)),
            )
            .collect();
        // A segment whose phase 1 failed runs here, from the copy opened for it above.
        let local = candidates(
            store,
            &items,
            &legs,
            |i, j| here(i, j) || in_scan_here.contains(&i),
            filter,
            shadow.len(),
            whole,
            stats,
            text,
        );
        // Each scan's segments carried beside it (code review): never recovered by filtering,
        // which would shift every later share's hits if a share were ever empty.
        let (scanned_targets, scans): (Vec<&Vec<usize>>, Vec<_>) = to_scan.into_iter().unzip();
        let phase_twos =
            futures_util::future::join_all(scans.into_iter().map(|scan| scan((**stats).clone())));
        let (local, seconds) = futures_util::future::join(local, phase_twos).await;
        Ok::<_, QueryError>((
            opened,
            local?,
            seconds,
            scanned_targets.into_iter().cloned().collect::<Vec<_>>(),
            stats.clone(),
        ))
    };
    // ⚠️ The one-exchange shares run beside ALL of it -- open, statistics and legs -- as M54's
    // did beside the open round: they wait for nothing here.
    let (main, shared) = futures_util::future::join(main, hits_started).await;
    let (opened, local, seconds, scanned_targets, stats) = main?;
    let stats = &stats;
    let mut candidates_found = local;
    // ⚠️ **Every failed share run here, and together** (M54, code review): a failed share
    // costs two rounds, never the answer -- and two rounds however many failed, which one
    // after another would make two per peer. A failed phase 2 runs with the global sum it
    // already has (M55, spec review): never the defaults.
    let mut failed_phased: Vec<(usize, Target)> = Vec::new();
    for (share, got) in scanned_targets.iter().zip(seconds) {
        match got {
            Ok(hits) => candidates_found.extend(hits),
            Err(_) => failed_phased.extend(
                share
                    .iter()
                    .filter_map(|i| targets.get(*i).map(|t| (*i, t.clone()))),
            ),
        }
    }
    let mut failed_hits: Vec<(usize, Target)> = Vec::new();
    for (share, got) in hit_targets.iter().zip(shared) {
        match got {
            Ok(hits) => candidates_found.extend(hits),
            Err(_) => failed_hits.extend(
                share
                    .iter()
                    .filter_map(|i| targets.get(*i).map(|t| (*i, t.clone()))),
            ),
        }
    }
    let (again_phased, again_hits) = futures_util::future::try_join(
        async {
            if failed_phased.is_empty() {
                Ok(Vec::new())
            } else {
                part(
                    store,
                    &failed_phased,
                    &all_legs,
                    filter,
                    shadow.len(),
                    stats,
                    text,
                    // M58 (spec review): under `sum` it runs whole and is cut by sum, as the
                    // peer would have -- never cut leg by leg at its limit.
                    match fusion {
                        Fusion::Sum { weights } => Some(SumCut {
                            weights,
                            keep: top_k.saturating_add(shadow.len()),
                        }),
                        _ => None,
                    },
                )
                .await
            }
        },
        async {
            if failed_hits.is_empty() {
                Ok(Vec::new())
            } else {
                part(
                    store,
                    &failed_hits,
                    &vector_legs,
                    filter,
                    shadow.len(),
                    stats,
                    text,
                    None,
                )
                .await
            }
        },
    )
    .await?;
    candidates_found.extend(again_phased);
    candidates_found.extend(again_hits);
    let candidates = candidates_found;

    if whole {
        return summed(
            store,
            targets,
            opened,
            candidates,
            runnable.len(),
            shadow,
            fusion,
            top_k,
        )
        .await;
    }

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

/// `Sum`'s end of `run` (M9g.2): the whole legs **fused before the shadow round**. Checking the
/// shadow on every candidate of a whole leg would fetch a block per matching row (spec review);
/// at most `|shadow|` of the fused rows can be shadowed -- an id has at most one live row across
/// the folded segments, because a fold supersedes every older row of an id it folds with a
/// delete vector (M9c.2) -- so the top `top_k + |shadow|` are resolved, the shadowed dropped,
/// and the rest cut to `top_k`. RRF and `max` keep the per-leg shadow check: dropping a row
/// shifts the others' ranks. No dense leg is allowed here,
/// so there is no `$dist`.
#[allow(
    clippy::too_many_arguments,
    reason = "the tail of `run`, taking what it built"
)]
async fn summed<S: BlobStore>(
    store: &S,
    targets: &[Target],
    opened: Vec<Opened>,
    candidates: Vec<(usize, usize, Vec<Hit>)>,
    leg_count: usize,
    shadow: &std::collections::HashSet<String>,
    fusion: Fusion,
    top_k: usize,
) -> Result<(Vec<Hit>, Vec<Opened>, Known, Dense), QueryError> {
    let mut legs: Vec<Vec<Hit>> = vec![Vec::new(); leg_count];
    for (_, j, hits) in candidates {
        if let Some(leg) = legs.get_mut(j) {
            leg.extend(hits);
        }
    }
    let mut fused = fuse(&legs, fusion, top_k.saturating_add(shadow.len()));
    let known = if shadow.is_empty() {
        Known::new()
    } else {
        let wanted: Vec<(usize, usize)> = fused.iter().map(|h| (h.segment, h.row)).collect();
        fetch_rows(store, targets, &opened, &wanted).await?
    };
    fused.retain(|h| {
        !targets.get(h.segment).is_some_and(|t| t.shadowed)
            || known
                .get(&(h.segment, h.row))
                .is_none_or(|d| !shadow.contains(&d.id))
    });
    fused.truncate(top_k);
    Ok((fused, opened, known, Dense::new()))
}

/// The ranking, **and the id of every hit**, in one more round than the ranking alone.
///
/// ⚠️ **Why this is here and not in the caller.** Resolving a hit needs the block that holds
/// its row, and only this function still has the opened segments: a caller doing it would
/// re-open each one — `Segment::open` then `ids_at`, two data-dependent rounds **per
/// segment**, serially. Measured that way at eight segments: **19 sequential round trips**
/// for one query, against D-34's budget. Here it is a single fan-out round whatever the
/// segment count, because the opens already happened and no block's address depends on
/// another's contents.
///
/// ⚠️ **It is a fourth round, and D-34 as restated by M47 allows it:** three to rank, one
/// to fetch what was ranked. A payload's address cannot be known before the ranking exists.
/// The way to three -- ids beside the vectors the leg already reads -- was priced by M42 at
/// +12.7% to +41.9% bytes, and the project's owner declined it.
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
#[allow(
    clippy::too_many_arguments,
    reason = "the query's parts, each needed by a leg"
)]
pub async fn query_rows_filtered<S: BlobStore + Clone>(
    store: &S,
    targets: &[Target],
    prefetch: &[Prefetch],
    text: &FullText,
    filter: Option<&Predicate>,
    shadow: &std::collections::HashSet<String>,
    fusion: Fusion,
    top_k: usize,
) -> Result<Vec<(Hit, Option<Document>, Option<f32>)>, QueryError> {
    query_rows_split(
        store,
        targets,
        prefetch,
        text,
        filter,
        shadow,
        fusion,
        top_k,
        Vec::new(),
    )
    .await
}

/// [`query_rows_filtered`], with the vector legs of some segments run by other servers
/// (M54). The answer is the one [`query_rows_filtered`] gives: each share is the same
/// per-segment work, merged as the coordinator merges its own.
///
/// # Errors
/// As [`query_rows_filtered`]. A share that fails is never an error: it is run here.
#[allow(
    clippy::too_many_arguments,
    reason = "the query's parts, each needed by a leg"
)]
pub async fn query_rows_split<S: BlobStore + Clone>(
    store: &S,
    targets: &[Target],
    prefetch: &[Prefetch],
    text: &FullText,
    filter: Option<&Predicate>,
    shadow: &std::collections::HashSet<String>,
    fusion: Fusion,
    top_k: usize,
    elsewhere: Vec<Elsewhere<'_>>,
) -> Result<Vec<(Hit, Option<Document>, Option<f32>)>, QueryError> {
    let (hits, opened, known, dense) = run(
        store, targets, prefetch, text, filter, shadow, fusion, top_k, elsewhere,
    )
    .await?;
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
    /// The same leg, owning its field and query (M59): what a leg's task carries.
    fn owned(&self) -> Prefetch {
        match *self {
            Self::Text {
                field,
                query,
                limit,
            } => Prefetch::Text {
                field: field.to_owned(),
                query: query.to_owned(),
                limit,
            },
            Self::Dense {
                field,
                query,
                limit,
                tune,
            } => Prefetch::Dense {
                field: field.to_owned(),
                query: query.to_vec(),
                limit,
                tune,
            },
            Self::Sparse {
                field,
                query,
                limit,
            } => Prefetch::Sparse {
                field: field.to_owned(),
                query: query.to_vec(),
                limit,
            },
        }
    }

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
        Segment::open_at(store, key, target.segment_len),
        maybe(store, centroids.clone().filter(|_| wants_dense)),
        maybe(
            store,
            (wants_sparse && target.sparse_dict).then(|| pstore_format::sparse::dict_key(key)),
        ),
        maybe(
            store,
            (wants_text && target.text_dict).then(|| pstore_format::text::dict_key(key)),
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

/// A query's leg tasks (M59), aborted if the query is dropped before they finish -- by a
/// client hanging up, or a peer's timeout -- so none reads and scores for nobody.
struct Tasks(Vec<tokio::task::JoinHandle<LegHits>>);

/// One `(segment, leg)` pair's candidates, or why there are none.
type LegHits = Result<(usize, usize, Vec<Hit>), QueryError>;

impl Tasks {
    /// Every task's result, in the order the tasks were made, as `try_join_all` returned the
    /// futures they replaced. Waited on as they finish (code review), so the first error to
    /// arrive is the query's and aborts the rest, as `try_join_all`'s did. A leg's panic is
    /// resumed here, as an inline one would have been.
    async fn join(mut self) -> Result<Vec<(usize, usize, Vec<Hit>)>, QueryError> {
        use futures_util::StreamExt as _;
        let mut slots: Vec<Option<(usize, usize, Vec<Hit>)>> = Vec::new();
        slots.resize_with(self.0.len(), || None);
        let mut pending: futures_util::stream::FuturesUnordered<_> = self
            .0
            .iter_mut()
            .enumerate()
            .map(|(k, task)| async move { (k, task.await) })
            .collect();
        while let Some((k, got)) = pending.next().await {
            let done = match got {
                Ok(done) => done?,
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                // Only `Drop` aborts, and it cannot run while this waits; a runtime shutting
                // down under the query is the one other way, and is an error.
                Err(_) => return Err(QueryError::Unimplemented("a leg cancelled")),
            };
            if let Some(slot) = slots.get_mut(k) {
                *slot = Some(done);
            }
        }
        drop(pending);
        Ok(slots.into_iter().flatten().collect())
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// One retriever's ranked answer.
async fn leg<S: BlobStore>(
    store: &S,
    key: &Key,
    opened: &Opened,
    r: &Runnable<'_>,
    segment: usize,
    stats: &Stats,
    text: &FullText,
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
            // ⚠️ A segment with no text index at all has no matching rows, and answers nothing
            // (M30): refusing it refused every text query over an index whose first fold had
            // no text, until a compaction happened to merge that segment away. Before the
            // sidecar check, which such a segment has nothing for.
            if opened.segment.text_fields().is_empty() {
                return Ok(Vec::new());
            }
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
            // M14: under the index's analyzer, scored with its `k1` and `b`.
            let terms = pstore_format::text::analyze(&text.analyzer, query);
            let hits = idx
                .search_with(store, key, &terms, stats, *limit, text.k1, text.b)
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
            // ⚠️ A segment with no sparse field has no matching rows, and answers nothing (M32),
            // judged by its footer and before the sidecar check, as M30 judged text: refusing
            // it refused every sparse query over an index with any such segment.
            if opened.segment.sparse_field().is_none() {
                return Ok(Vec::new());
            }
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
