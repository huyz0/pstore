//! Running the legs — concurrently, over one open segment.

use crate::fuse::{Fusion, Hit, fuse};
use pstore_blob::{BlobStore, Key};
use pstore_format::{FormatError, Segment};
use pstore_index::sparse::SparseIndex;
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
    /// Full-text — **in the shape, not in the engine**.
    ///
    /// ⚠️ Present so a caller's request does not have to change when the retriever arrives
    /// (D-73), and refused by name when executed. Dropping it silently would answer with a
    /// plausible ranking computed from fewer retrievers than were asked for.
    Text {
        /// The field to search.
        field: String,
        /// The query text.
        query: String,
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

/// Which sidecars a query needs, derived from the legs.
struct Opened {
    segment: Segment,
    centroids: Option<bytes::Bytes>,
    dictionary: Option<bytes::Bytes>,
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
    key: &Key,
    centroids: &Key,
    prefetch: &[Prefetch],
    fusion: Fusion,
    top_k: usize,
) -> Result<Vec<Hit>, QueryError> {
    // Refused BEFORE any I/O. A request naming a retriever we cannot run is not a request we
    // half-answer.
    for p in prefetch {
        if let Prefetch::Text { .. } = p {
            return Err(QueryError::Unimplemented("text"));
        }
    }

    let opened = open(store, key, centroids, prefetch).await?;
    let legs =
        futures_util::future::try_join_all(prefetch.iter().map(|p| leg(store, key, &opened, p)))
            .await?;
    Ok(fuse(&legs, fusion, top_k))
}

/// The open round: the footer and every sidecar any leg needs, together.
async fn open<S: BlobStore>(
    store: &S,
    key: &Key,
    centroids: &Key,
    prefetch: &[Prefetch],
) -> Result<Opened, QueryError> {
    let wants_dense = prefetch.iter().any(|p| matches!(p, Prefetch::Dense { .. }));
    let wants_sparse = prefetch
        .iter()
        .any(|p| matches!(p, Prefetch::Sparse { .. }));
    // ⚠️ Three futures, one round. Every key is derived and none depends on another's
    // contents, so awaiting them in sequence would cost a round trip per modality — which is
    // exactly what `prefetch[]` must not turn into.
    let (segment, cen, dict) = futures_util::future::join3(
        Segment::open(store, key),
        maybe(store, wants_dense.then(|| centroids.clone())),
        maybe(
            store,
            wants_sparse.then(|| pstore_format::sparse::dict_key(key)),
        ),
    )
    .await;
    Ok(Opened {
        segment: segment?,
        centroids: cen,
        dictionary: dict,
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
    p: &Prefetch,
) -> Result<Vec<Hit>, QueryError> {
    match p {
        Prefetch::Text { .. } => Err(QueryError::Unimplemented("text")),
        Prefetch::Dense {
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
                .map(|(row, score)| Hit { row, score })
                .collect())
        }
        Prefetch::Sparse {
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
                .map(|(row, score)| Hit { row, score })
                .collect())
        }
    }
}
