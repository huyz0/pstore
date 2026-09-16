//! The storage engine: HEAD and the commit protocol, WAL lanes carrying cross-index
//! bundles, and the memtable that makes a write visible before it is folded.

mod bundle;
mod head;
pub mod lanes;

pub use bundle::Entry;
pub use head::{Head, HeadAt, SegmentRef};

use pstore_blob::{BlobStore, Key};
use pstore_format::{Document, Filter, Segment};
use pstore_types::{Epoch, LaneId, Seq, TenantId};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The key one lane's bundle lives at. **Derived**, so a successor computes it rather than
/// discovering it — which is what keeps recovery off the LIST path.
#[must_use]
pub fn bundle_key(tenant: TenantId, lane: LaneId, seq: Seq) -> Key {
    Key::new(format!(
        "{:04x}/wal/{}/{:016x}/{:016}.bundle",
        tenant.0 as u16, tenant.0, lane.0, seq.0
    ))
}

/// How many times a commit rebases before giving up.
///
/// Bounded on purpose: a commit that cannot land after this many rebases is reporting
/// contention the caller should know about, not something to keep paying for silently.
const MAX_COMMIT_ATTEMPTS: u32 = 24;

/// Why an engine operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// A query over the segments HEAD names.
    ///
    /// ⚠️ A string, like [`Self::Format`] beside it, because `EngineError` is `Clone + Eq` and
    /// `QueryError` is neither. A caller that needs the typed error calls `pstore_query::query`
    /// directly — which is public, and is what this method composes.
    #[error("query: {0}")]
    Query(String),

    /// Another committer won. **Rebase and retry.**
    #[error("commit lost: another writer won")]
    Lost,
    /// The backend could not evaluate the condition. **Retry without rebasing.**
    #[error("commit contended: retry without rebasing")]
    Contended,
    /// HEAD did not decode.
    #[error("HEAD is corrupt")]
    CorruptHead,
    /// A WAL bundle did not decode.
    #[error("WAL bundle is corrupt")]
    CorruptBundle,
    /// A segment could not be read.
    #[error("format error: {0}")]
    Format(String),
    /// The blob store could not serve it.
    #[error("blob error: {0}")]
    Blob(String),
    /// The backend's recorded profile says it cannot fence, so nothing may be told it is
    /// durable.
    ///
    /// ⚠️ The one error here that is **policy rather than failure**: nothing went wrong, and
    /// that is the point. A backend whose CAS is `Divergent` returns *success* on a write
    /// that did not fence, so the only moment this can be caught is before the write. Names
    /// the backend and the primitive, because "unsupported backend" leaves an operator with
    /// nothing to do.
    #[error(
        "backend {backend} cannot fence: {primitive} is {observed} - refusing to write anything that would be reported durable"
    )]
    BackendCannotFence {
        /// The profile's backend label.
        backend: String,
        /// Which primitive is not `Supported`.
        primitive: &'static str,
        /// What was observed of it.
        observed: String,
    },
    /// The query vector's dimension is not the indexed field's.
    ///
    /// ⚠️ **Typed, and the only query failure that is**, because it is the one a *client*
    /// causes and can fix. Everything else `pstore_query` reports is a corrupt object or an
    /// unimplemented retriever, which is ours. Added in M7c: the server had no way to answer
    /// a wrong-sized vector with anything but `500 internal`, which tells a caller its
    /// request was unsound when in fact it was merely wrong in a nameable way.
    #[error("the query vector has {got} dimensions and the field has {expected}")]
    DimensionMismatch {
        /// What the segment's field layout records.
        expected: usize,
        /// What the query carried.
        got: usize,
    },
    /// A lane's tail could not be found within the probe bound.
    ///
    /// Not "the lane is too long" in practice — it means the store kept answering, which
    /// is a backend fault or a corrupted lane rather than a large one.
    #[error("lane {0:?} did not terminate within the probe bound")]
    LaneTooLong(pstore_types::LaneId),
}

impl From<pstore_blob::BlobError> for EngineError {
    fn from(e: pstore_blob::BlobError) -> Self {
        Self::Blob(e.to_string())
    }
}

impl From<pstore_format::FormatError> for EngineError {
    fn from(e: pstore_format::FormatError) -> Self {
        Self::Format(e.to_string())
    }
}

/// Refuses a backend whose recorded profile says it cannot fence.
///
/// ⚠️ **Called at the doors *and* at the CAS**, which is the same pair `write` already forms
/// with `check_storable` at the door and `try_finish` at the far end. The door call is what
/// makes the refusal cost zero requests — `gc` in particular `delete_batch`es before it
/// commits, so a guard only at the CAS would let it destroy objects and then refuse. The CAS
/// call is what stops a path added later from committing around the doors.
///
/// The corpus says "fail loudly at startup". There is no startup object here — `Engine::new`
/// is infallible across every one of its call sites — so the door is where it lands, which is
/// strictly earlier than any corruption. `Capabilities::admits_durable_writes` is public so a
/// server, when one exists, can refuse sooner.
pub(crate) fn require_fencing<S: BlobStore + ?Sized>(store: &S) -> Result<(), EngineError> {
    let caps = store.capabilities();
    match caps.first_divergence() {
        None => Ok(()),
        Some((primitive, observed)) => Err(EngineError::BackendCannotFence {
            backend: caps.backend.clone(),
            primitive,
            observed: format!("{observed:?}"),
        }),
    }
}

/// Rows written but not yet folded, held in memory and served by queries.
///
/// This is the freshness layer: **visibility does not wait on the fold**, so the flush
/// interval never enters the time-to-searchable budget.
#[derive(Debug, Default)]
struct Memtable {
    /// Buffered but not yet in a bundle.
    pending: BTreeMap<String, Vec<Document>>,
    /// Flushed to a bundle, durable, still not folded into a segment.
    durable: BTreeMap<String, Vec<Document>>,
    /// Bumped whenever either map changes, so a cached fresh segment knows it is stale.
    ///
    /// ⚠️ `flush` moves rows from `pending` to `durable` without changing what they are, so it
    /// invalidates needlessly. Bumping on it anyway is the safe direction and is cheaper than
    /// reasoning about which mutations matter.
    generation: u64,
}

/// Two stores behind one `BlobStore`: the tenant's durable one, and the private in-memory one
/// holding the fresh segment.
///
/// ⚠️ **Routed by key prefix, not by trying both.** Trying the fresh store first and falling
/// back would turn every miss into two requests against the tenant's store, and trying the
/// durable one first would let a fresh key 404 before it was ever looked for. The fresh
/// segment's keys all begin `mem/`, which no derived tenant key does — they begin with the
/// tenant's four hex digits.
#[derive(Debug)]
struct Split<S> {
    durable: Arc<S>,
    fresh: Option<Arc<pstore_blob::MemoryStore>>,
}

impl<S: BlobStore> Split<S> {
    fn is_fresh(key: &Key) -> bool {
        key.as_str().starts_with("mem/")
    }
}

macro_rules! split_to {
    ($self:ident, $key:expr, $call:ident ( $($arg:expr),* )) => {
        match (&$self.fresh, Split::<S>::is_fresh($key)) {
            (Some(f), true) => f.$call($($arg),*).await,
            _ => $self.durable.$call($($arg),*).await,
        }
    };
}

#[async_trait::async_trait]
impl<S: BlobStore> BlobStore for Split<S> {
    fn capabilities(&self) -> &pstore_blob::Capabilities {
        self.durable.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        split_to!(self, key, get(key))
    }
    async fn get_range(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        split_to!(self, key, get_range(key, range))
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        split_to!(self, key, get_suffix(key, n))
    }
    async fn get_with_tag(
        &self,
        key: &Key,
    ) -> Result<(bytes::Bytes, pstore_types::CasTag), pstore_blob::BlobError> {
        split_to!(self, key, get_with_tag(key))
    }
    async fn get_tag(
        &self,
        key: &Key,
    ) -> Result<Option<pstore_types::CasTag>, pstore_blob::BlobError> {
        split_to!(self, key, get_tag(key))
    }
    async fn head(&self, key: &Key) -> Result<u64, pstore_blob::BlobError> {
        split_to!(self, key, head(key))
    }
    async fn put(
        &self,
        key: &Key,
        body: bytes::Bytes,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::BlobError> {
        split_to!(self, key, put(key, body))
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: bytes::Bytes,
        pre: pstore_blob::Precondition,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::CasError> {
        split_to!(self, key, put_conditional(key, body, pre))
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), pstore_blob::BlobError> {
        self.durable.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, pstore_blob::BlobError> {
        self.durable.list_unrestricted(prefix).await
    }
    // ⚠️ The classed reads are defaulted on the trait and every decorator must forward them, or
    // the class is dropped and D-21 is disabled with every test still passing.
    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: pstore_blob::Class,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        split_to!(self, key, get_range_as(key, range, class))
    }
    async fn get_suffix_as(
        &self,
        key: &Key,
        n: u64,
        class: pstore_blob::Class,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        split_to!(self, key, get_suffix_as(key, n, class))
    }
    async fn get_immutable(
        &self,
        key: &Key,
        class: pstore_blob::Class,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        split_to!(self, key, get_immutable(key, class))
    }
}

/// One index's memtable, sealed into a segment that lives only in memory.
#[derive(Debug)]
struct Fresh {
    /// The memtable generation this was built from.
    generation: u64,
    /// Which index it holds.
    index: String,
    /// The private store the segment and its sidecars live in.
    store: Arc<pstore_blob::MemoryStore>,
    /// Where in that store.
    target: pstore_query::Target,
    /// The documents, in the segment's row order — which the clustering decides, so it is not
    /// the order they were written in.
    rows: Vec<Document>,
}

/// What [`Engine::query`] returns: hits, plus the documents behind the fresh ordinal.
///
/// ⚠️ **A struct rather than a bare `Vec<Hit>`**, because `Hit { segment: unfolded_at, .. }`
/// indexes **nothing in HEAD**. A caller resolving it against the segment list would name a
/// different document, plausibly.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    /// The fused ranking. Every `segment` below [`Self::unfolded_at`] indexes HEAD's segment
    /// list for the index queried.
    pub hits: Vec<pstore_query::Hit>,
    /// The unfolded documents, in the row order [`Self::unfolded_at`]'s hits index.
    pub unfolded: Vec<Document>,
    /// The ordinal the freshness layer occupies — one past HEAD's last segment.
    ///
    /// ⚠️ When nothing is unfolded no fresh segment is built at all, so no hit carries this
    /// and [`Self::unfolded`] is empty. An empty segment would occupy an ordinal for nothing.
    pub unfolded_at: usize,
    /// The id of every hit, in `hits` order. `None` only if the row vanished under us.
    ///
    /// ⚠️ **Carried, because resolving it later costs round trips that the budget does not
    /// have.** A caller re-opening each segment to look a row up pays two data-dependent
    /// rounds per segment — measured at **19 sequential round trips** for an eight-segment
    /// index, against D-34's three. Resolved inside the query, it is one fan-out round
    /// however many segments there are.
    pub ids: Vec<Option<String>>,
    /// The segments HEAD named for this index **when the answer was computed**, in the order
    /// every `Hit::segment` below [`Self::unfolded_at`] indexes.
    ///
    /// ⚠️ **Carried rather than re-read, and that is a request-count property, not a
    /// convenience.** Resolving hits against a second read of HEAD would cost a round trip
    /// *and* race a fold: between the two reads the segment list can be renumbered, so hit
    /// 3 would name a different document — plausibly, and with nothing to notice it. It is
    /// also what lets a caller tell "this index does not exist" from "it matched nothing",
    /// without asking again.
    pub segments: Vec<SegmentRef>,
}

/// What HEAD knows about one index, without reading a single segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStats {
    /// Segments the index is made of.
    pub segments: u64,
    /// Rows across those segments. ⚠️ **Folded rows only** — the freshness layer is a
    /// property of one process and is reported separately, by `pending_indexes`.
    pub documents: u64,
    /// The epoch HEAD was at when this was read.
    pub epoch: Epoch,
}

/// One writer's view of one tenant.
#[derive(Debug)]
pub struct Engine<S> {
    store: Arc<S>,
    tenant: TenantId,
    lane: LaneId,
    mem: Mutex<Memtable>,
    /// Next sequence in this lane. Lanes are single-writer, so this needs no coordination
    /// with anyone — which is the entire point of lanes.
    seq: Mutex<Seq>,
    /// Serialises this lane's flushes against each other.
    ///
    /// ⚠️ Not contention control — a lane is single-writer by definition. This *enforces*
    /// that definition against the one caller who ignores it: two overlapping `flush`
    /// calls on the same handle would each reserve a sequence and race to write them, and
    /// a failure of the lower one would leave a gap the tail probe stops at. Cheap to
    /// hold, because the thing it excludes should never happen.
    flushing: tokio::sync::Mutex<()>,
    committed: Mutex<Epoch>,
    /// The memtable sealed into an in-memory segment, and the generation it was built from.
    ///
    /// ⚠️ **The memtable becomes a SEGMENT rather than being scored separately**, so there is
    /// one BM25, one dense path, one sparse path, one fusion and one `(segment, row)` space.
    /// Scoring raw documents instead would have meant a second implementation of each that
    /// must agree with the first exactly — the hazard `Hit`'s own doc names about inventing an
    /// identity twice.
    ///
    /// ⚠️ Cached on the generation counter so the cost lands on `write` and `flush` rather
    /// than on every query, and held in a **private** `MemoryStore` so sealing it costs the
    /// tenant's store nothing.
    fresh: tokio::sync::Mutex<Option<Fresh>>,
    /// What the dense index is built with.
    ///
    /// ⚠️ **`exact_scan_threshold` is the load-bearing one.** Below it nothing is clustered,
    /// no centroid object is written, and the segment's row order is the order documents were
    /// written in — which is every fold at the sizes anything is tested at. Above it rows are
    /// written in **list order**, so a probe is one contiguous ranged read, and the row order
    /// is therefore no longer the write order.
    params: pstore_index::cluster::Params,
    /// The attribute this writer's text index is built over.
    ///
    /// ⚠️ **Used by `fold` and never by `compact`.** A compaction re-analyzes the original
    /// attribute — postings cannot be inverted back into text — so a handle left on the
    /// default merging a `body` index would rebuild it over `"text"`, find no strings, and
    /// write no postings. The merged segment would carry every row and no text index, with
    /// nothing reporting an error. `compact` takes the name from its inputs instead.
    text_field: String,
}

/// The ABA guard: a value that never repeats for two different commits.
///
/// ⚠️ **Load-bearing on any backend whose CAS tag is content-derived** — an S3 ETag on a
/// single-part PUT is the MD5 of the body. Two HEADs that happen to encode identically
/// would then carry identical tags, so a writer that read the first, paused, and woke
/// after the world changed and changed back would have its CAS *accepted*. The nonce makes
/// two commits byte-different even when everything else about them matches.
///
/// XOR, not OR or AND: both of those lose information, so distinct `(epoch, lane)` pairs
/// collapse onto the same nonce and the guard silently stops guarding. Mutation testing
/// found `|` and `&` indistinguishable from `^` to every test in the workspace, which is
/// why `nonces_never_collide_across_epochs_and_lanes` exists.
#[must_use]
pub fn nonce_for(epoch: Epoch, lane: LaneId) -> u64 {
    epoch.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ lane.0
}

/// How long a writer waits before retrying a lost commit.
///
/// ⚠️ Jittered, and that is not decoration. Without it optimistic concurrency
/// **livelocks**: every loser retries immediately, collides with the same peers, and
/// loses again. It surfaced as a *flaky* test rather than a failing one, which is the
/// more expensive way to find out. The jitter comes from the lane rather than a random
/// source so a failing schedule still replays.
///
/// ⚠️ Separated from the sleep on purpose. While this was one function its entire
/// contents were invisible to every test — mutation testing replaced the whole body with
/// nothing, inverted the shift, and swapped the arithmetic, and not one test noticed,
/// because the only observable was elapsed microseconds. A pure function has a contract
/// that can be stated and checked; a sleep does not.
#[must_use]
pub fn backoff_delay(lane: LaneId, attempt: u32) -> std::time::Duration {
    // Doubling, capped: the cap stops a late retry waiting far longer than the operation
    // it is retrying.
    let base = 1u64 << attempt.min(BACKOFF_CAP_SHIFT);
    // 1..=8, never 0: a jitter that can be zero leaves the lanes that draw it colliding
    // in lockstep, which is the livelock this exists to prevent.
    let jitter = (lane.0 % JITTER_SPREAD) + 1;
    std::time::Duration::from_micros(base * jitter)
}

/// Where the doubling stops.
const BACKOFF_CAP_SHIFT: u32 = 6;
/// How many distinct delays a given attempt can produce.
const JITTER_SPREAD: u64 = 8;

async fn backoff(lane: LaneId, attempt: u32) {
    tokio::time::sleep(backoff_delay(lane, attempt)).await;
}

impl<S: BlobStore> Engine<S> {
    /// A writer on one lane of one tenant.
    pub fn new(store: Arc<S>, tenant: TenantId, lane: LaneId) -> Self {
        Self {
            store,
            tenant,
            lane,
            mem: Mutex::new(Memtable::default()),
            seq: Mutex::new(Seq::ZERO),
            flushing: tokio::sync::Mutex::new(()),
            committed: Mutex::new(Epoch::ZERO),
            fresh: tokio::sync::Mutex::new(None),
            // ⚠️ **`replicas: 0`, and it is a measured default rather than the clamp it
            // replaces.** At `Query::default()`'s probe width of 8, replication buys 0.0000
            // recall on clustered data (0.9810 either way) and costs bytes (0.841 against
            // 0.769); on uniform data it matches probing wider while costing 1.99x the stored
            // codes. It earns its keep only at small `p`, which nothing here runs at.
            // `cargo run --release -p pstore-index --example recall -- --replicas`.
            params: pstore_index::cluster::Params {
                replicas: 0,
                boundary: 0.0,
                ..pstore_index::cluster::Params::default()
            },
            text_field: pstore_format::text::DEFAULT_TEXT_FIELD.to_owned(),
        }
    }

    /// Builds this writer's dense index with `params` rather than the defaults.
    ///
    /// Exists so a test can reach the clustered path without writing 25,000 rows, and so a
    /// deployment can tune list size without a rebuild. Per **engine**, not per index —
    /// per-index parameters are the schema question M6c deferred.
    #[must_use]
    pub fn with_index_params(mut self, params: pstore_index::cluster::Params) -> Self {
        self.params = params;
        self
    }

    /// Builds this writer's text index over `name` rather than the default.
    ///
    /// ⚠️ **The format and engine half of a schema, not the schema.** Nothing tenant-facing
    /// sets this, and there is no way to change it on an index that already has segments —
    /// those are the write-side policy questions and they need the server that does not
    /// exist. What this buys is that a segment built over `body` says so, and every reader
    /// is told rather than assuming.
    #[must_use]
    pub fn with_text_field(mut self, name: &str) -> Self {
        self.text_field = name.to_owned();
        self
    }

    fn mem(&self) -> std::sync::MutexGuard<'_, Memtable> {
        self.mem
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The last epoch this engine committed.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        *self
            .committed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lane_key(&self, seq: Seq) -> Key {
        bundle_key(self.tenant, self.lane, seq)
    }

    fn segment_key(&self, epoch: Epoch, index: &str) -> Key {
        Key::new(format!(
            "{:04x}/tnt/{}/idx/{index}/seg/L0/{:020}-{:016x}.seg",
            self.tenant.0 as u16, self.tenant.0, epoch.0, self.lane.0
        ))
    }

    /// Writes one segment, its dense index, and every sidecar the index needs.
    ///
    /// ⚠️ **One place.** Fold and compaction both seal segments, and a field written by one
    /// and not the other is an index whose rows lose it the first time they are merged — with
    /// every test that only folds still passing.
    ///
    /// ⚠️ **One builder, not three.** `try_build_all` builds the dense clustering, the sparse
    /// postings and the text postings together because none of them is independent: the
    /// clustering decides the segment's **row order**, and the other two address those rows.
    /// Three builders called over the input order produce three internally consistent indexes
    /// pointing at three different documents.
    ///
    /// ⚠️ `try_build_all`, not `build_all`. `build_all` seals with `finish`, which is lossy
    /// for anything the format cannot hold — and the fold used exactly that, so a document the
    /// writer could not store was written as nothing and reported as durable. The refusal this
    /// keeps is `try_finish`'s **index-budget** one, which `check_storable` at the write door
    /// knows nothing about.
    async fn seal(
        &self,
        key: &Key,
        docs: &[Document],
        text_field: &str,
    ) -> Result<(), EngineError> {
        let sparse = sparse_field_of(docs);
        let wants_text = docs.iter().any(|d| {
            matches!(
                d.attrs.get(text_field),
                Some(pstore_format::Value::Str(s)) if !s.is_empty()
            )
        });
        // ⚠️ **The `replicas: 0` clamp is gone (M3c).** It was a correctness clamp, not a
        // tuning choice: rows were written in list order, so a replicated vector was written
        // to the segment TWICE — 400 documents folded and merged came back as 431 rows, and
        // it compounded on every merge against `Engine::scan`'s "exactly once". The codes and
        // the documents now live in separate row spaces, so replication duplicates the former
        // and not the latter.
        //
        // ⚠️ **This turns nothing on.** `Engine::new` still defaults to `replicas: 0`, because
        // at the default probe width of 8 replication buys **0.0000** recall and costs bytes
        // (0.841 MB against 0.769). What it buys is query bytes at small `p` — 0.9610 @ 0.288
        // MB at p=2 — and lowering `p` is a decision with its own measurement.
        let built = pstore_index::vec_index::try_build_all(
            docs,
            self.params,
            pstore_format::DEFAULT_FIELD,
            sparse.as_deref(),
            wants_text.then_some(text_field),
        )
        .map_err(|e| EngineError::Format(e.to_string()))?;

        // ⚠️ Every sidecar FIRST, and before the segment is named by HEAD. A segment whose
        // sidecar is not there yet reads as a segment whose field cannot be reconstructed —
        // postings intact, and unreachable.
        //
        // ⚠️ Written only when there is something in it. `Built` carries a dictionary whenever
        // the field was named, and an object per fold for an index that has no postings is a
        // request and an object that never reads back.
        if let Some(d) = built.dictionary.filter(|d| !d.is_empty()) {
            self.store
                .put(&pstore_format::sparse::dict_key(key), bytes::Bytes::from(d))
                .await?;
        }
        if let Some(d) = built.text_dictionary.filter(|d| !d.is_empty()) {
            self.store
                .put(&pstore_format::text::dict_key(key), bytes::Bytes::from(d))
                .await?;
        }
        // ⚠️ Absent is not an error: D-10 reads a missing centroid table as "this index is
        // below the exact-scan threshold, scan me exactly".
        if let Some(c) = &built.centroids {
            self.store
                .put(
                    &pstore_index::vec_index::centroid_key(key),
                    bytes::Bytes::from(c.encode()),
                )
                .await?;
        }
        self.store.put(key, built.segment).await?;
        Ok(())
    }

    /// Buffers documents. **Visible immediately**; durable at the next [`Self::flush`].
    pub async fn write(&self, index: &str, docs: Vec<Document>) -> Result<(), EngineError> {
        // ⚠️ Refused at the DOOR, not at the fold. The document model expresses named,
        // plural and sparse fields; the segment layout stores one dense vector until M3b.3.
        // Accepting a document here and discovering at fold time that it cannot be stored
        // means acknowledging a write that will never be visible — and before this check
        // existed, such a document was written as *nothing*, silently.
        for d in &docs {
            pstore_format::check_storable(d).map_err(|e| EngineError::Format(e.to_string()))?;
        }
        let mut m = self.mem();
        // ⚠️ **One index, one width**, checked against the rows this process already holds
        // for it — buffered or flushed-but-unfolded. Costs no request, because the answer is
        // in memory or it is not knowable for free at all.
        //
        // ⚠️ Found through the API: a four-dimensional document and a two-dimensional one,
        // written in **separate batches**, were both accepted, and the short one then
        // outranked an exact match. A per-batch check is the case that never mattered.
        //
        // ⚠️ What this cannot see is a process that has just started, or an index whose rows
        // have all been folded: the width then lives in the segment, and reading it would put
        // a blob request on the write path. That case is **loud at query time** instead — the
        // dense leg takes its dimension from the segment's field layout and refuses a query
        // that does not match. Silent wrongness is what had to go; a refusal one step later
        // is a cost.
        let known = m
            .pending
            .get(index)
            .and_then(|rows| rows.first())
            .or_else(|| m.durable.get(index).and_then(|rows| rows.first()))
            .map(|d| d.vector().len());
        if let Some(expected) = known
            && let Some(odd) = docs.iter().find(|d| d.vector().len() != expected)
        {
            return Err(EngineError::DimensionMismatch {
                expected,
                got: odd.vector().len(),
            });
        }
        m.generation += 1;
        m.pending.entry(index.to_owned()).or_default().extend(docs);
        Ok(())
    }

    /// The indexes this process holds unfolded rows for, in name order.
    ///
    /// ⚠️ **Not the same question as [`Self::indexes`], and a caller needs both.** HEAD names
    /// what is folded; this names what is written and queryable through the freshness layer
    /// but not yet in any segment. An index exists if either says so — deciding on HEAD alone
    /// would report a `404` for a document the very next query returns.
    ///
    /// It is a property of **this process**: another writer's memtable is invisible here, and
    /// that is the honest answer rather than a limitation, because nothing durable records it.
    pub async fn pending_indexes(&self) -> Vec<String> {
        let m = self.mem();
        let mut names: Vec<String> = m.pending.keys().chain(m.durable.keys()).cloned().collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Every index HEAD names for this tenant, in name order. **One read, no LIST.**
    ///
    /// # Errors
    /// If HEAD cannot be read. An absent HEAD is an empty list, not an error: a tenant that
    /// has never committed owns no indexes, which is a fact rather than a failure.
    pub async fn indexes(&self) -> Result<Vec<String>, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        Ok(at.head.indexes.into_keys().collect())
    }

    /// Segment count, document count and epoch for one index, or `None` if HEAD does not
    /// name it. **One read**, and never a walk of the segments: `SegmentRef` already carries
    /// its row count, so counting documents costs nothing beyond the manifest.
    ///
    /// # Errors
    /// If HEAD cannot be read.
    pub async fn index_stats(&self, index: &str) -> Result<Option<IndexStats>, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        Ok(at.head.indexes.get(index).map(|refs| IndexStats {
            segments: refs.len() as u64,
            documents: refs.iter().map(|r| u64::from(r.rows)).sum(),
            epoch: at.head.epoch,
        }))
    }

    /// Turns an [`Answer`]'s hits into `(id, score)` pairs. **Issues no requests.**
    ///
    /// ⚠️ The ids were resolved inside the query, in the round that had the segments open.
    /// This was a method that re-opened every segment and read a block from each, serially —
    /// two data-dependent rounds per segment on top of the query's three, which at eight
    /// segments is 19. Code review measured it; the fix is that the work moved rather than
    /// that it got faster.
    #[must_use]
    pub fn resolve(&self, answer: &Answer) -> Vec<(String, f32)> {
        answer
            .hits
            .iter()
            .enumerate()
            .filter_map(|(i, h)| {
                let id = if h.segment == answer.unfolded_at {
                    answer.unfolded.get(h.row).map(|d| d.id.clone())
                } else {
                    answer.ids.get(i).cloned().flatten()
                };
                id.map(|id| (id, h.score))
            })
            .collect()
    }

    /// The ids currently buffered and not yet acknowledged.
    ///
    /// Exposed so a recovery test can state precisely which rows a `flush` acknowledges.
    /// Guessing that from the outside would make the test's own bookkeeping the thing
    /// under test.
    #[doc(hidden)]
    pub async fn pending_for_test(&self) -> Vec<String> {
        self.mem()
            .pending
            .values()
            .flatten()
            .map(|d| d.id.clone())
            .collect()
    }

    /// Writes everything buffered as **one bundle object**, whatever it covers.
    ///
    /// `RA = 1 W` for the batch, and for every index in it.
    pub async fn flush(&self) -> Result<Option<Seq>, EngineError> {
        require_fencing(&*self.store)?;
        let _lane = self.flushing.lock().await;
        let pending = {
            let mut m = self.mem();
            m.generation += 1;
            if m.pending.is_empty() {
                return Ok(None);
            }
            std::mem::take(&mut m.pending)
        };
        let seq = *self
            .seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Registered once, on the lane's first flush. A lane nobody can find is a lane
        // whose writes cannot be recovered, and the registration is what makes a successor
        // able to discover it without being told.
        if seq == Seq::ZERO
            && let Err(e) = lanes::register(&*self.store, self.tenant, self.lane).await
        {
            self.restore(pending);
            return Err(e);
        }
        // The one PUT.
        if let Err(e) = self
            .store
            .put(&self.lane_key(seq), bundle::encode(&pending).into())
            .await
        {
            // ⚠️ **The sequence is not consumed, and this is load-bearing** (OQ-91).
            //
            // A lane is recovered by probing forward from the last watermark until a key
            // is missing, so a lane must be DENSE: the first absent sequence is taken as
            // the end. Burning a number on a failed write punches a permanent hole, and
            // every bundle after it — all of them acknowledged, all of them durable —
            // becomes invisible to every future reader. One refused PUT silently
            // truncates the lane forever.
            //
            // Found by the OQ-91 scenario losing two acknowledged rows on seed 0, not by
            // reading this code.
            self.restore(pending);
            return Err(e.into());
        }
        {
            let mut s = self
                .seq
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *s = seq.next();
        }
        // Only now does it move from pending to durable: a write that failed to land must
        // not be reported as durable, and must stay visible so it is not lost.
        let mut m = self.mem();
        m.generation += 1;
        for (idx, docs) in pending {
            m.durable.entry(idx).or_default().extend(docs);
        }
        Ok(Some(seq))
    }

    /// Puts a failed flush's rows back in front of anything written since.
    ///
    /// Without this a refused flush would drop the rows it took, which is a silent loss of
    /// data the caller was never told was durable — worse than the error it reports,
    /// because the caller's retry would have no way to know what to retry.
    fn restore(&self, pending: BTreeMap<String, Vec<Document>>) {
        let mut m = self.mem();
        m.generation += 1;
        for (idx, mut docs) in pending {
            let slot = m.pending.entry(idx).or_default();
            // Older rows first: they were written first, and a later write to the same id
            // must stay later.
            docs.append(slot);
            *slot = docs;
        }
    }

    /// Replays **every lane's** unfolded WAL bundles into segments and commits them.
    ///
    /// ⚠️ Tenant-scoped, not lane-scoped, and that is the point: folding is work done *on
    /// behalf of the tenant*, so any node may do it and a successor can fold a dead
    /// writer's lane without ever having spoken to it. The lanes come from the registry
    /// and the tails from forward probing — nothing is listed, and nothing is injected.
    ///
    /// ⚠️ **Reads the bundles from the blob store, not from memory.** Folding the in-memory
    /// copy would make the WAL write-only: the objects would be paid for and never read,
    /// and a process that restarted could not recover a single acknowledged write.
    pub async fn fold(&self) -> Result<Epoch, EngineError> {
        require_fencing(&*self.store)?;
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let at = head::read(&*self.store, self.tenant).await?;
            let live = lanes::live(&*self.store, self.tenant).await?;

            // Each lane's unfolded span, discovered rather than remembered.
            let spans = futures_util::future::try_join_all(live.iter().map(|l| {
                let from = at.head.watermarks.get(&l.0).copied().unwrap_or(0);
                async move {
                    lanes::tail(&*self.store, self.tenant, *l, from)
                        .await
                        .map(|tail| (*l, from, tail))
                }
            }))
            .await?;

            let keys: Vec<(LaneId, Key)> = spans
                .iter()
                .flat_map(|(lane, from, tail)| {
                    (*from..*tail).map(move |n| (*lane, bundle_key(self.tenant, *lane, Seq(n))))
                })
                .collect();
            if keys.is_empty() {
                return Ok(at.head.epoch);
            }

            let bodies =
                futures_util::future::try_join_all(keys.iter().map(|(_, k)| self.store.get(k)))
                    .await?;

            let mut by_index: BTreeMap<String, Vec<Document>> = BTreeMap::new();
            for body in &bodies {
                for (name, entry) in bundle::read_index(body)? {
                    by_index
                        .entry(name)
                        .or_default()
                        .extend(bundle::read_entry(body, &entry)?);
                }
            }
            if by_index.is_empty() {
                return Ok(at.head.epoch);
            }

            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            // The ABA guard: two HEADs differing only in content a content-derived tag is
            // computed from would otherwise share a tag. Derived from the epoch and lane,
            // so it is deterministic and needs no clock.
            next.nonce = nonce_for(next.epoch, self.lane);

            // One segment per index. Folding every index into one object would make each
            // index's ref point at the whole thing, and a scan would return its
            // neighbours' rows.
            for (idx, docs) in &by_index {
                let seg_key = self.segment_key(next.epoch, idx);
                self.seal(&seg_key, docs, &self.text_field).await?;
                next.indexes
                    .entry(idx.clone())
                    .or_default()
                    .push(SegmentRef {
                        key: seg_key.as_str().to_owned(),
                        rows: docs.len() as u32,
                    });
            }
            // ⚠️ Advanced only for the spans actually folded. Advancing a lane past
            // bundles this attempt did not read would drop them permanently — and nothing
            // downstream could tell, because the watermark is the only record of what is
            // outstanding.
            //
            // ⚠️ The `> 0` is a **size** guard, not a correctness one, and `> -> >=` is a
            // provably equivalent mutant: both read sites treat an absent watermark as zero
            // (`unwrap_or(0)` here, `is_some_and(|w| *w > seq.0)` in `head.rs`), so writing
            // `lane -> 0` is indistinguishable from writing nothing. What it buys is a HEAD
            // that does not grow a 16-byte entry per idle lane on every fold. Recorded so a
            // sweep does not spend a round trying to kill it with a test that would only pin
            // HEAD's encoded length.
            for (lane, _, tail) in &spans {
                if *tail > 0 {
                    next.watermarks.insert(lane.0, *tail);
                }
            }
            // The bundles just folded are now garbage: their rows live in a segment HEAD
            // names. Recorded here rather than deleted here, because a reader holding the
            // previous epoch may still be replaying them.
            next.graveyard
                .entry(next.epoch.0)
                .or_default()
                .extend(keys.iter().map(|(_, k)| k.as_str().to_owned()));

            match head::commit(&*self.store, self.tenant, &at, &next).await {
                Ok(epoch) => {
                    let mut m = self.mem();
                    m.generation += 1;
                    m.durable.clear();
                    drop(m);
                    *self
                        .committed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = epoch;
                    return Ok(epoch);
                }
                Err(EngineError::Lost | EngineError::Contended)
                    if attempt < MAX_COMMIT_ATTEMPTS - 1 =>
                {
                    backoff(self.lane, attempt).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(EngineError::Lost)
    }

    /// Reaps objects dereferenced more than `retention` epochs ago.
    ///
    /// **Zero LIST.** GC works from the manifest's graveyard, which records each key at
    /// the epoch it stopped being referenced. Enumerating the bucket would answer a
    /// different question — what *exists*, rather than what is still reachable — and cost
    /// a PUT per thousand keys to answer it wrongly.
    ///
    /// `retention` is a number of epochs, not a duration. A reader that read HEAD at
    /// epoch *e* may take arbitrarily long to finish scanning, so what protects it is not
    /// elapsed time but the guarantee that nothing referenced at *e* is reaped until the
    /// tenant has committed `retention` further epochs.
    ///
    /// Returns how many objects were reaped.
    pub async fn gc(&self, retention: u64) -> Result<usize, EngineError> {
        require_fencing(&*self.store)?;
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let at = head::read(&*self.store, self.tenant).await?;
            // Everything dereferenced at an epoch this old is beyond the reach of any
            // reader the window promises to protect.
            let horizon = at.head.epoch.0.saturating_sub(retention);
            let due: Vec<u64> = at
                .head
                .graveyard
                .range(..=horizon)
                .map(|(e, _)| *e)
                .collect();
            if due.is_empty() {
                return Ok(0);
            }
            let live: std::collections::BTreeSet<&str> = at
                .head
                .indexes
                .values()
                .flatten()
                .map(|r| r.key.as_str())
                .collect();
            let doomed: Vec<Key> = due
                .iter()
                .filter_map(|e| at.head.graveyard.get(e))
                .flatten()
                // ⚠️ Checked against what HEAD names *now*, not against what it named when
                // the key was buried. Cheap, and the one thing standing between a bug
                // anywhere in the commit path and deleting live data.
                .filter(|k| !live.contains(k.as_str()))
                .map(|k| Key::new(k.clone()))
                .collect();
            // ⚠️ And the sidecars beside each SEGMENT. The graveyard records segments and
            // bundles alike; a sidecar is reachable only by derivation from a segment, so one
            // reaped without its dictionaries leaves objects nothing can ever name again. The
            // deletes are unconditional for a segment because the alternative is a HEAD
            // request per key to find out — but a bundle never has one, and deriving them for
            // every key would triple the batch and inflate the reaped count with objects that
            // never existed.
            let mut batch = doomed.clone();
            for k in &doomed {
                if k.as_str().ends_with(".seg") {
                    batch.push(pstore_format::sparse::dict_key(k));
                    batch.push(pstore_format::text::dict_key(k));
                }
            }

            // ⚠️ Deleted BEFORE the manifest is pruned, and the order is not arbitrary.
            // Pruning first and then failing to delete loses the only record that these
            // objects exist, and they leak with nothing left to find them by. Deleting
            // first and then failing to prune costs a repeated delete on the next pass,
            // which is idempotent.
            //
            // ⚠️ **Chunked at the backend's cap, which is read and never assumed.**
            // `BlobStore::delete_batch` is documented "capped at `max_batch_delete`" and both
            // implementations enforce it, while this list is the graveyard's and has no bound
            // at all — so a tenant with more dead objects than one batch holds could not be
            // collected on any backend. S3 takes 1000 per request and Azure 256; a constant
            // here would be wrong on one of them.
            let cap = self.store.capabilities().max_batch_delete.max(1);
            for chunk in batch.chunks(cap) {
                self.store.delete_batch(chunk).await?;
            }

            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            next.nonce = nonce_for(next.epoch, self.lane);
            for e in &due {
                next.graveyard.remove(e);
            }
            match head::commit(&*self.store, self.tenant, &at, &next).await {
                Ok(_) => return Ok(doomed.len()),
                Err(EngineError::Lost | EngineError::Contended)
                    if attempt < MAX_COMMIT_ATTEMPTS - 1 =>
                {
                    backoff(self.lane, attempt).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(EngineError::Lost)
    }

    /// Commits an arbitrary edit to HEAD.
    ///
    /// Exists for one reason: GC's live-reference guard refuses to reap a key HEAD still
    /// names, and the commit protocol is supposed to make that state unreachable. A
    /// defence that cannot be reached cannot be tested, and an untested defence is one
    /// that quietly stops working — so a test is allowed to construct the state the
    /// protocol forbids, and check that GC survives it.
    #[doc(hidden)]
    pub async fn commit_head_for_test(
        &self,
        mutate: impl FnOnce(&mut Head),
    ) -> Result<Epoch, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        let mut next = at.head.clone();
        next.epoch = next.epoch.next();
        next.nonce = nonce_for(next.epoch, self.lane);
        mutate(&mut next);
        head::commit(&*self.store, self.tenant, &at, &next).await
    }

    /// The published manifest, so a test can assert on what is *referenced* rather than
    /// inferring it from what a scan happens to return.
    #[doc(hidden)]
    pub async fn head_for_test(&self) -> Head {
        head::read(&*self.store, self.tenant)
            .await
            .map(|at| at.head)
            .unwrap_or_default()
    }

    /// The key a compactor writes its merged output to.
    ///
    /// ⚠️ **Carries the compactor's lane**, so two nodes compacting the same inputs write
    /// to two different objects. Deriving the key from the inputs instead would be
    /// tempting — the losers would cost nothing — but it makes the second compactor
    /// overwrite a live object unconditionally, which is Invariant I1 gone. The
    /// create-if-absent that would fix it is exactly the precondition MinIO was *measured*
    /// ignoring, so the fix would be silently absent on a backend we support. A wasted
    /// object that GC reaps is the cheaper mistake.
    fn compacted_key(&self, epoch: Epoch, index: &str) -> Key {
        Key::new(format!(
            "{:04x}/tnt/{}/idx/{index}/seg/L1/{:020}-{:016x}.seg",
            self.tenant.0 as u16, self.tenant.0, epoch.0, self.lane.0
        ))
    }

    /// Merges an index's segments into one, and publishes it by CAS.
    ///
    /// **Optimistic, not coordinated.** Any node may compact any index at any time; there
    /// is no lock, no lease and no claim, because there is nothing to protect. Several
    /// nodes may do the same merge concurrently: they read the same inputs, write their
    /// own outputs, and race to publish. Exactly one CAS lands. The losers discard, and
    /// their objects are unreferenced from the moment they lose, so GC reaps them without
    /// needing to know a compaction ever happened.
    ///
    /// Returns `None` when there was nothing to do — fewer than two segments, or another
    /// compactor got there first.
    ///
    /// `RA = n Rpar + 1 W + 1 commit`, for any *n*.
    pub async fn compact(&self, index: &str) -> Result<Option<Epoch>, EngineError> {
        require_fencing(&*self.store)?;
        let at = head::read(&*self.store, self.tenant).await?;
        let inputs: Vec<SegmentRef> = at.head.indexes.get(index).cloned().unwrap_or_default();
        if inputs.len() < 2 {
            return Ok(None);
        }
        let keys: Vec<Key> = inputs.iter().map(|r| Key::new(r.key.clone())).collect();

        // Opened and scanned together, like every other multi-segment read: n inputs are
        // n parallel fetches, not n round trips.
        let opened =
            futures_util::future::try_join_all(keys.iter().map(|k| Segment::open(&*self.store, k)))
                .await?;
        let scanned = futures_util::future::try_join_all(
            opened
                .iter()
                .zip(&keys)
                .map(|(seg, k)| seg.scan(&*self.store, k, None)),
        )
        .await?;
        // In input order, so the merged segment reads back in the order the inputs would
        // have. A merge that reorders is a merge that changes the answer.
        let rows: Vec<Document> = scanned.into_iter().flatten().collect();

        // ⚠️ From the INPUTS, never from this handle. A compaction re-analyzes the original
        // attribute, so a handle on the default merging a `body` index would rebuild it over
        // `"text"` and write no postings at all — a text index destroyed by a merge, every
        // row intact, nothing reporting an error.
        //
        // ⚠️ Disagreement is REFUSED, not resolved. Picking one input's name rebuilds the
        // other's rows over an attribute they do not carry, which is the same silent
        // destruction one segment at a time. Nothing can produce disagreeing inputs today;
        // that is what makes now the cheap time to shut the door.
        let mut named: Vec<&str> = opened
            .iter()
            .flat_map(|s| s.text_fields().iter().map(String::as_str))
            .collect();
        named.sort_unstable();
        named.dedup();
        let text_field = match named.as_slice() {
            [] => pstore_format::text::DEFAULT_TEXT_FIELD,
            [one] => one,
            many => {
                return Err(EngineError::Format(format!(
                    "cannot merge segments whose text indexes name different attributes: \
                     {}. Rebuilding over one of them writes no postings for the rows that \
                     carry the other",
                    many.join(", ")
                )));
            }
        };

        let out_key = self.compacted_key(at.head.epoch.next(), index);
        // The single W (two, for an index with a sparse field). Written BEFORE the commit and
        // never rewritten on a retry: a rebase changes which HEAD we condition on, not what
        // we merged.
        self.seal(&out_key, &rows, text_field).await?;
        let out = SegmentRef {
            key: out_key.as_str().to_owned(),
            rows: rows.len() as u32,
        };

        let mut at = at;
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let current: Vec<SegmentRef> = at.head.indexes.get(index).cloned().unwrap_or_default();
            // ⚠️ The discard condition. If any input is no longer named by HEAD, another
            // compactor published this merge and ours is stale — republishing it would
            // resurrect rows that a later fold may already have superseded. Losing is the
            // normal outcome of optimistic work, so it is not an error.
            if !inputs
                .iter()
                .all(|i| current.iter().any(|c| c.key == i.key))
            {
                return Ok(None);
            }

            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            next.nonce = nonce_for(next.epoch, self.lane);
            // Segments added since we read: kept, in place, after the merged one. Dropping
            // them would silently discard every row folded while we were merging.
            let mut kept: Vec<SegmentRef> = vec![out.clone()];
            kept.extend(
                current
                    .iter()
                    .filter(|c| !inputs.iter().any(|i| i.key == c.key))
                    .cloned(),
            );
            next.indexes.insert(index.to_owned(), kept);
            // Same rule for the segments this merge replaced.
            next.graveyard
                .entry(next.epoch.0)
                .or_default()
                .extend(inputs.iter().map(|i| i.key.clone()));

            match head::commit(&*self.store, self.tenant, &at, &next).await {
                Ok(epoch) => return Ok(Some(epoch)),
                Err(e @ (EngineError::Lost | EngineError::Contended))
                    if attempt < MAX_COMMIT_ATTEMPTS - 1 =>
                {
                    if matches!(e, EngineError::Lost) {
                        at = head::read(&*self.store, self.tenant).await?;
                    }
                    backoff(self.lane, attempt).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(EngineError::Lost)
    }

    /// Every row of an index a filter selects: committed segments plus everything not yet
    /// folded, each appearing **exactly once**.
    pub async fn scan(
        &self,
        index: &str,
        filter: Option<&Filter>,
    ) -> Result<Vec<Document>, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        let keys: Vec<Key> = at
            .head
            .indexes
            .get(index)
            .into_iter()
            .flatten()
            .map(|r| Key::new(r.key.clone()))
            .collect();

        // ⚠️ Segments are opened TOGETHER, then scanned together. A loop over segment refs
        // is functionally identical and turns a ten-segment index into a twenty-one-hop
        // query -- measured, not guessed -- which is six times the whole latency budget.
        // Width is free; depth is not.
        let opened =
            futures_util::future::try_join_all(keys.iter().map(|k| Segment::open(&*self.store, k)))
                .await?;
        let scanned = futures_util::future::try_join_all(
            opened
                .iter()
                .zip(&keys)
                .map(|(seg, k)| seg.scan(&*self.store, k, filter)),
        )
        .await?;
        let mut out: Vec<Document> = scanned.into_iter().flatten().collect();
        // Unfolded rows live only in memory. They are not in any segment, so there is
        // nothing to deduplicate against -- the fold clears them in the same step that
        // publishes the segment.
        let m = self.mem();
        for src in [&m.durable, &m.pending] {
            for d in src.get(index).into_iter().flatten() {
                if filter.is_none_or(|f| f.matches(d)) {
                    out.push(d.clone());
                }
            }
        }
        Ok(out)
    }

    /// Seals this index's unfolded rows into a segment that lives only in memory.
    ///
    /// ⚠️ Returns `None` when nothing is unfolded — an **empty** fresh segment would occupy an
    /// ordinal for nothing, and every other ordinal is a number a caller resolves against HEAD.
    ///
    /// ⚠️ A refusal here **fails the query**. Falling back to the folded-only answer would be
    /// silently returning the stale result this exists to remove, which is worse than an error
    /// a caller can see.
    async fn fresh_target(&self, index: &str) -> Result<Option<pstore_query::Target>, EngineError> {
        let (generation, rows) = {
            let m = self.mem();
            let mut rows: Vec<Document> = Vec::new();
            for src in [&m.durable, &m.pending] {
                rows.extend(src.get(index).into_iter().flatten().cloned());
            }
            (m.generation, rows)
        };
        let mut slot = self.fresh.lock().await;
        if let Some(f) = slot.as_ref()
            && f.generation == generation
            && f.index == index
        {
            return Ok(Some(f.target.clone()));
        }
        if rows.is_empty() {
            *slot = None;
            return Ok(None);
        }

        let store = Arc::new(pstore_blob::MemoryStore::new());
        let key = Key::new(format!("mem/{index}.seg"));
        // The same builder a fold uses, so the fresh half and the folded half are scored by
        // the same code rather than by two that must agree.
        let sparse = sparse_field_of(&rows);
        let wants_text = rows.iter().any(|d| {
            matches!(
                d.attrs.get(self.text_field.as_str()),
                Some(pstore_format::Value::Str(s)) if !s.is_empty()
            )
        });
        let built = pstore_index::vec_index::try_build_all(
            &rows,
            self.params,
            pstore_format::DEFAULT_FIELD,
            sparse.as_deref(),
            wants_text.then_some(self.text_field.as_str()),
        )
        .map_err(|e| EngineError::Format(e.to_string()))?;

        if let Some(d) = built.dictionary.filter(|d| !d.is_empty()) {
            store
                .put(
                    &pstore_format::sparse::dict_key(&key),
                    bytes::Bytes::from(d),
                )
                .await?;
        }
        if let Some(d) = built.text_dictionary.filter(|d| !d.is_empty()) {
            store
                .put(&pstore_format::text::dict_key(&key), bytes::Bytes::from(d))
                .await?;
        }
        if let Some(c) = &built.centroids {
            store
                .put(
                    &pstore_index::vec_index::centroid_key(&key),
                    bytes::Bytes::from(c.encode()),
                )
                .await?;
        }
        store.put(&key, built.segment).await?;

        let target = pstore_query::Target {
            centroids: pstore_index::vec_index::centroid_key(&key),
            segment: key,
        };
        // In the segment's row order, which the clustering decides — not the order written.
        let ordered: Vec<Document> = built
            .order
            .iter()
            .filter_map(|r| rows.get(*r).cloned())
            .collect();
        *slot = Some(Fresh {
            generation,
            index: index.to_owned(),
            store,
            target: target.clone(),
            rows: ordered,
        });
        Ok(Some(target))
    }

    /// Runs `prefetch` over every segment HEAD names for `index` **and over the rows not yet
    /// folded**, fused into one answer.
    ///
    /// ⚠️ **The freshness layer is in the answer.** `Memtable`'s doc promises that "visibility
    /// does not wait on the fold"; until M5h this method was the one read path that did wait.
    /// The unfolded rows are sealed into a segment that lives only in memory and queried
    /// alongside HEAD's, so one BM25, one dense path and one fusion cover both halves.
    ///
    /// ⚠️ Hits at [`Answer::unfolded_at`] index [`Answer::unfolded`], **not** HEAD's segment
    /// list. Every other ordinal indexes HEAD's, for this call only: a fold or a compaction
    /// between two calls renumbers them, which is why M5f scoped that identity to a snapshot.
    ///
    /// # Errors
    /// If HEAD cannot be read, a segment or sidecar cannot be, or the unfolded rows cannot be
    /// sealed — the last fails the query rather than quietly answering without them.
    pub async fn query(
        &self,
        index: &str,
        prefetch: &[pstore_query::Prefetch],
        fusion: pstore_query::Fusion,
        top_k: usize,
    ) -> Result<Answer, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        // ⚠️ Derived, never discovered: a segment's centroid table is at its own key, and
        // absent means "below the exact-scan threshold" rather than missing (D-10).
        let refs: Vec<SegmentRef> = at.head.indexes.get(index).cloned().unwrap_or_default();
        let mut targets: Vec<pstore_query::Target> = refs
            .iter()
            .map(|r| {
                let segment = Key::new(r.key.clone());
                pstore_query::Target {
                    centroids: pstore_index::vec_index::centroid_key(&segment),
                    segment,
                }
            })
            .collect();
        let unfolded_at = targets.len();

        let fresh = self.fresh_target(index).await?;
        let held = self.fresh.lock().await;
        let (unfolded, fresh_store) = match (&fresh, held.as_ref()) {
            (Some(_), Some(f)) => (f.rows.clone(), Some(Arc::clone(&f.store))),
            _ => (Vec::new(), None),
        };
        drop(held);
        if let Some(t) = fresh {
            targets.push(t);
        }
        if targets.is_empty() {
            return Ok(Answer {
                hits: Vec::new(),
                ids: Vec::new(),
                unfolded,
                unfolded_at,
                segments: refs,
            });
        }

        // ⚠️ Two stores, one query: the folded segments live in the tenant's and the fresh one
        // in a private `MemoryStore`, so sealing it costs the tenant's store nothing. `Split`
        // routes each key to the one that holds it.
        let store = Split {
            durable: Arc::clone(&self.store),
            fresh: fresh_store,
        };
        let resolved = pstore_query::query_ids(&store, &targets, prefetch, fusion, top_k)
            .await
            .map_err(|e| match e {
                pstore_query::QueryError::Format(
                    pstore_format::FormatError::DimensionMismatch { expected, got },
                ) => EngineError::DimensionMismatch { expected, got },
                other => EngineError::Query(other.to_string()),
            })?;
        let (hits, ids) = resolved.into_iter().unzip();
        Ok(Answer {
            hits,
            ids,
            unfolded,
            unfolded_at,
            segments: refs,
        })
    }

    /// Every folded segment's rows, in row order, for a test that resolves a hit.
    ///
    /// # Errors
    /// If HEAD, a segment or its blocks cannot be read.
    #[doc(hidden)]
    pub async fn segment_rows_for_test(
        &self,
        index: &str,
    ) -> Result<Vec<Vec<Document>>, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        let mut out = Vec::new();
        for r in at.head.indexes.get(index).into_iter().flatten() {
            let k = Key::new(r.key.clone());
            let seg = pstore_format::Segment::open(&*self.store, &k).await?;
            out.push(seg.scan(&*self.store, &k, None).await?);
        }
        Ok(out)
    }

    /// Exact k-nearest neighbours across everything the index contains.
    pub async fn search(
        &self,
        index: &str,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
    ) -> Result<Vec<(String, f32)>, EngineError> {
        let docs = self.scan(index, filter).await?;
        let mut scored: Vec<(String, f32)> = docs
            .into_iter()
            .map(|d| {
                let dist = d
                    .vector()
                    .iter()
                    .zip(query)
                    .map(|(a, b)| (a - b) * (a - b))
                    .sum::<f32>();
                (d.id, dist)
            })
            .collect();
        scored.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(k);
        Ok(scored)
    }

    /// Attempts a commit against a **deliberately stale** view of HEAD.
    ///
    /// Exists so the fencing property can be asserted: a caller holding a superseded tag
    /// must be refused. Not part of the engine's real API.
    #[doc(hidden)]
    pub async fn commit_stale_for_test(&self) -> Result<Epoch, EngineError> {
        let stale = HeadAt {
            head: Head::default(),
            tag: None,
        };
        let next = Head {
            epoch: Epoch(1),
            ..Head::default()
        };
        head::commit(&*self.store, self.tenant, &stale, &next).await
    }
}

/// The one sparse field a batch of documents carries, if any.
///
/// ⚠️ Returns the **first in name order** so a segment is deterministic. A second sparse
/// field needs its own section id pair, the way `FieldVectors` mirrors `Vectors`; until then
/// it would silently share the first one's postings, which is a merge of two fields into one.
fn sparse_field_of(docs: &[Document]) -> Option<String> {
    let mut names: Vec<&str> = docs
        .iter()
        .flat_map(|d| {
            d.vectors.iter().filter_map(|(n, f)| {
                matches!(f, pstore_format::VectorField::Sparse(_)).then_some(n.as_str())
            })
        })
        .collect();
    names.sort_unstable();
    names.first().map(|s| (*s).to_owned())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod split_tests {
    use super::*;
    use pstore_blob::MemoryStore;

    /// ⚠️ **Backlog row 20.** Every other method of `Split` is routed by a caller that would
    /// notice the wrong arm; `head` has no caller at all, so replacing its body with a
    /// constant survived M6i's sweep. The mutant is *inert* until something asks the
    /// decorator for a size — this is that something, as a test, and the two objects have
    /// **different lengths** so a constant and a forwarded-to-`durable` mutant both fail.
    #[tokio::test]
    async fn the_split_store_routes_head_by_key_prefix() {
        let durable = Arc::new(MemoryStore::new());
        let fresh = Arc::new(MemoryStore::new());
        let d_key = Key::new("0007/seg/0".to_owned());
        let f_key = Key::new("mem/seg/0".to_owned());
        durable
            .put(&d_key, bytes::Bytes::from_static(b"durable-object"))
            .await
            .unwrap();
        fresh
            .put(&f_key, bytes::Bytes::from_static(b"fresh"))
            .await
            .unwrap();

        let split = Split {
            durable: Arc::clone(&durable),
            fresh: Some(Arc::clone(&fresh)),
        };
        assert_eq!(split.head(&d_key).await.unwrap(), 14);
        assert_eq!(split.head(&f_key).await.unwrap(), 5);

        // ⚠️ And with no fresh store the `mem/` key goes to the durable one, which is the
        // `_` arm of the routing macro: absent there, so it must be an error rather than a
        // silent zero.
        let only_durable = Split::<MemoryStore> {
            durable: Arc::clone(&durable),
            fresh: None,
        };
        assert!(only_durable.head(&f_key).await.is_err());
    }
}
