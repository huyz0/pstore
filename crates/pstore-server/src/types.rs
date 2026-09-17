//! The wire shapes — `api-design.md`'s request and response bodies, minus what M7c defers.
//!
//! ⚠️ `vector` is a JSON array of `f32`. D-36 says vectors must always be sendable as raw
//! binary and that JSON encoding of a 768-dim vector is ~10 KB against 3 KB raw, which is a
//! top-three CPU cost. Accepted for v1 by the same document that names it; the field keeps
//! its name so a base64 variant is an addition rather than a move.

use serde::{Deserialize, Serialize};

/// What a request cost the tenant, in blob requests and bytes.
///
/// ⚠️ **This request's own**, as a difference of the tenant's monotone counters — see
/// `Api::spend`. Exact for one request in flight, cross-attributed under concurrency.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Cost {
    /// Read-class requests.
    pub blob_reads: u64,
    /// Write-class requests. A PUT costs 12.5 GETs, which is why this is reported separately.
    pub blob_writes: u64,
    /// LISTs. **Always zero**, and reported so that a regression is visible to the caller
    /// rather than only to a test.
    pub blob_lists: u64,
    /// Bytes read.
    pub bytes_read: u64,
    /// Bytes written.
    pub bytes_written: u64,
}

/// How durable a write must be before it is acknowledged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// Buffered in this process, visible to its queries, **not yet in the blob store**.
    #[default]
    Batched,
    /// Written as one bundle object before the response. `RA = 1 W` for the batch.
    Durable,
}

/// One document, as a client sends it.
#[derive(Debug, Clone, Deserialize)]
pub struct DocumentIn {
    /// The client's identifier.
    pub id: String,
    /// The dense vector.
    pub vector: Vec<f32>,
    /// Free-text attribute, indexed for BM25 when the engine's text field names it.
    #[serde(default)]
    pub text: Option<String>,
}

/// `PUT /v1/indexes/{id}/documents`.
#[derive(Debug, Clone, Deserialize)]
pub struct WriteRequest {
    /// Defaults to `batched`. ⚠️ An unknown value is **refused**, never downgraded.
    #[serde(default)]
    pub durability: Durability,
    /// The batch. Batch-first: the cheap thing is the natural thing.
    pub documents: Vec<DocumentIn>,
}

/// The answer to a write.
#[derive(Debug, Clone, Serialize)]
pub struct WriteResponse {
    /// This engine's last **committed** epoch. ⚠️ Not a read of HEAD: a write costs the
    /// requests the cost model says it costs, and asking the store what epoch it is on would
    /// add one to every batch.
    pub epoch: u64,
    /// Documents accepted.
    pub documents_written: usize,
    /// Whether the batch is in the blob store or only in this process.
    pub durable: bool,
    /// What it cost.
    pub cost: Cost,
}

/// `POST /v1/indexes/{id}/query`.
#[derive(Debug, Clone, Deserialize)]
pub struct QueryRequest {
    /// The dense leg's query vector.
    #[serde(default)]
    pub vector: Option<Vec<f32>>,
    /// The dense leg's field. Defaults to the engine's default vector field.
    #[serde(default)]
    pub field: Option<String>,
    /// The BM25 leg's query text.
    #[serde(default)]
    pub text: Option<String>,
    /// Answer as the index stood at this epoch. ⚠️ Bounded by what GC has reaped: below the
    /// horizon the request is **refused** rather than answered with a short index.
    #[serde(default)]
    pub as_of: Option<u64>,
    /// How many results. **Zero is refused**: a query that can return nothing is a request
    /// nobody meant to make.
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

fn default_top_k() -> usize {
    10
}

/// One ranked result.
#[derive(Debug, Clone, Serialize)]
pub struct ResultRow {
    /// The document's id.
    pub id: String,
    /// Its fused score.
    pub score: f32,
}

/// What the answer cost and how fresh it is.
#[derive(Debug, Clone, Serialize)]
pub struct QueryMeta {
    /// The epoch the answer was computed against.
    pub epoch: u64,
    /// Hits served from the freshness layer rather than from a segment.
    ///
    /// ⚠️ The honest half of `api-design.md` principle 3: a caller can see that an answer
    /// came from rows this process has not folded, and therefore that another process would
    /// not have returned it.
    pub unfolded_hits: usize,
    /// What it cost.
    pub cost: Cost,
}

/// The answer to a query.
#[derive(Debug, Clone, Serialize)]
pub struct QueryResponse {
    /// The ranking.
    pub results: Vec<ResultRow>,
    /// Cost and freshness.
    pub meta: QueryMeta,
}

/// What an index's rows must look like. Inferred by its first fold, immutable afterwards.
#[derive(Debug, Clone, Serialize)]
pub struct Schema {
    /// Components in every vector.
    pub dims: u32,
    /// The attribute the text index is built over, or empty if the index carries no text.
    pub text_field: String,
}

/// One index, as `GET /v1/indexes/{id}` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct IndexSummary {
    /// Its name.
    pub index: String,
    /// Segments HEAD names for it.
    pub segments: u64,
    /// Rows in those segments. ⚠️ Folded rows only; `unfolded` is the other half.
    pub documents: u64,
    /// HEAD's epoch.
    pub epoch: u64,
    /// Whether this process holds unfolded rows for it.
    pub unfolded: bool,
    /// What its rows must look like. `null` until its first fold records it.
    pub schema: Option<Schema>,
    /// ⚠️ Rows that were **acknowledged and then discarded** at a fold because they
    /// contradicted the schema. Reported because a discard nobody can see is indistinguishable
    /// from a bug — and because the alternative to discarding was stopping the tenant.
    pub rejected_rows: u64,
    /// What the summary cost.
    pub cost: Cost,
}

/// `GET /v1/indexes`.
#[derive(Debug, Clone, Serialize)]
pub struct IndexList {
    /// Every index HEAD names, **unioned with this process's unfolded ones**, in name order.
    pub indexes: Vec<String>,
    /// What the enumeration cost. Zero LISTs, which is the claim worth reporting.
    pub cost: Cost,
}

/// `POST /v1/admin/gc`.
#[derive(Debug, Clone, Serialize)]
pub struct GcResponse {
    /// The epoch the reap committed.
    pub epoch: u64,
    /// Objects deleted.
    pub reaped: usize,
    /// ⚠️ The oldest epoch `as_of` can still answer. Reported because a caller that has been
    /// time-travelling needs to learn where its history now ends, and the only alternative to
    /// telling it is letting it find out by being refused.
    pub reaped_before: u64,
    /// What it cost.
    pub cost: Cost,
}

/// `POST /v1/admin/fold`.
#[derive(Debug, Clone, Serialize)]
pub struct FoldResponse {
    /// The epoch the fold committed.
    pub epoch: u64,
    /// What it cost.
    pub cost: Cost,
}

/// The body of every refusal.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    /// The refusal.
    pub error: ErrorDetail,
}

impl ErrorBody {
    pub(crate) fn new(code: &'static str, message: String, retryable: bool) -> Self {
        Self {
            error: ErrorDetail {
                code,
                message,
                retryable,
            },
        }
    }
}

/// A stable code, a human message, and whether retrying may help.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorDetail {
    /// Stable across releases; the field a client switches on.
    pub code: &'static str,
    /// For a human reading a log.
    pub message: String,
    /// ⚠️ Derived from the status, so no two rows of the table can disagree.
    pub retryable: bool,
}
