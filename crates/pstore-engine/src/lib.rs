//! The storage engine: HEAD and the commit protocol, WAL lanes carrying cross-index
//! bundles, and the memtable that makes a write visible before it is folded.

mod bundle;
mod head;
pub mod lanes;

pub use bundle::Entry;
pub use head::{Head, HeadAt, IndexSchema, Metric, SegmentRef, TimeTravel};

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
    /// A vector the index's metric cannot measure (M9d), as a document or a query: a zero
    /// vector under `cosine_distance`, or one whose squared norm overflows `f32`. A client's
    /// error, typed so it can be answered as one.
    #[error("{0}: the vector's norm is zero or too large for the index's distance metric")]
    Unmeasurable(String),
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
    /// The rows contradict the index's recorded schema.
    ///
    /// ⚠️ Raised at the **door** and at the **flush**, never at the fold: a fold is
    /// all-or-nothing across every index in its bundle set, so failing it would stop every
    /// later fold for the whole tenant. The fold drops and counts instead.
    #[error("index {index}: {what} is {got}, and the index's schema says {expected}")]
    SchemaConflict {
        /// The index whose schema was contradicted.
        index: String,
        /// Which fact — `"the vector width"` or `"the text field"`.
        what: &'static str,
        /// What the schema records.
        expected: String,
        /// What the rows carry.
        got: String,
    },
    /// The epoch asked for cannot be reconstructed.
    #[error(transparent)]
    TimeTravel(#[from] head::TimeTravel),
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
    /// Buffered but not yet in a bundle -- **including rows whose bundle PUT is in flight**
    /// (M9c.1, BACKLOG row 35): a flush moves them only once the PUT has succeeded, so a query
    /// during the PUT still sees them.
    pending: BTreeMap<String, Vec<Document>>,
    /// Flushed batches not yet known to be folded, each with its bundle's lane sequence.
    ///
    /// ⚠️ **Tagged, so a fold removes exactly what it folded** (M9c.1, rows 36 and 38). A
    /// fold used to clear the whole map, including a batch flushed after it probed the lane --
    /// durable, acknowledged, and invisible until the next fold; and another process folding
    /// this lane left the rows here, served twice. Now a batch is dropped only when a HEAD's
    /// watermark for this lane is past its sequence, whoever folded it.
    durable: Vec<(u64, BTreeMap<String, Vec<Document>>)>,
    /// Every batch below this sequence is known folded and has been dropped. A query holding a
    /// HEAD whose watermark is below it would be missing rows its HEAD does not have yet, and
    /// re-reads HEAD instead.
    pruned: u64,
    /// Bumped whenever the rows change, so a cached fresh segment knows it is stale.
    generation: u64,
}

impl Memtable {
    /// Buffers rows, and moves the generation so a cached fresh segment knows it is stale.
    ///
    /// ⚠️ One step shared by `write` and its test-only twin (M8g): each used to carry its own
    /// copy of the bump, so a mutant in the twin could survive while the real one was tested.
    fn buffer(&mut self, index: &str, docs: Vec<Document>) {
        self.generation += 1;
        self.pending
            .entry(index.to_owned())
            .or_default()
            .extend(docs);
    }

    /// The flushed rows of `index`, oldest batch first.
    fn durable_rows<'a>(&'a self, index: &'a str) -> impl Iterator<Item = &'a Document> + 'a {
        self.durable
            .iter()
            .flat_map(move |(_, batch)| batch.get(index).into_iter().flatten())
    }

    /// Drops every batch a HEAD with this lane `watermark` has folded (its sequence is below
    /// it), and reports whether the rows are now consistent with that HEAD: `false` when an
    /// earlier call already dropped batches this HEAD has not folded yet.
    fn prune(&mut self, watermark: u64) -> bool {
        // No `watermark > pruned` guard: a batch below `pruned` is one a flush pushed after a
        // fold had already folded it (see `flush_inner`), and dropping it is right whichever
        // watermark finds it; above `pruned`, an older watermark's `retain` removes nothing.
        let before = self.durable.len();
        self.durable.retain(|(seq, _)| *seq >= watermark);
        if self.durable.len() != before {
            self.generation += 1;
        }
        self.pruned = self.pruned.max(watermark);
        self.pruned <= watermark
    }
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
    /// Where in that store; `None` when every unfolded operation is a delete.
    target: Option<pstore_query::Target>,
    /// The documents, in the segment's row order — which the clustering decides, so it is not
    /// the order they were written in.
    rows: Vec<Document>,
    /// Every id with an unfolded operation, upsert or delete: the rows they supersede in the
    /// index's segments are hidden (M9c.2). ⚠️ From the SAME memtable snapshot as `rows`, so a
    /// write landing between the two cannot show both versions of an id.
    shadow: std::collections::HashSet<String>,
    /// The metric the unfolded rows were written under, when there are any (M9d): what a query
    /// of an index with no schema yet transforms by.
    metric: Option<Metric>,
}

/// The attribute name marking a tombstone (M9c.2): **empty**, which the write door refuses and
/// `Engine::write` refuses too, so no client can forge one.
const TOMBSTONE: &str = "";

/// A delete of `id`, as an operation in the same ordered log as writes (M9c.2).
fn tombstone(id: String) -> Document {
    Document {
        id,
        vectors: BTreeMap::new(),
        attrs: BTreeMap::from([(TOMBSTONE.to_owned(), pstore_format::Value::Int(0))]),
    }
}

/// Whether an operation is a delete.
fn is_tombstone(d: &Document) -> bool {
    d.attrs.contains_key(TOMBSTONE)
}

/// The reserved attribute a row carries its metric in, from the write to the fold (M9d).
/// Stripped before anything is sealed; absent means `dot_product`.
const METRIC_ATTR: &str = "$metric";

/// The metric a row was written under.
fn metric_of(d: &Document) -> Metric {
    match d.attrs.get(METRIC_ATTR) {
        Some(pstore_format::Value::Int(code)) => Metric::from_code(*code).unwrap_or_default(),
        _ => Metric::DotProduct,
    }
}

/// The row as it is sealed and served: without its metric.
fn stripped(mut d: Document) -> Document {
    d.attrs.remove(METRIC_ATTR);
    d
}

/// `v` as `metric` stores it (M9d), or `None` when the metric cannot measure it: a zero
/// vector under cosine has no direction, and -- code review -- a finite vector whose squared
/// norm overflows `f32` would be stored as zeros under cosine and with a `-inf` component
/// under euclidean.
fn transform_stored(metric: Metric, v: &[f32]) -> Option<Vec<f32>> {
    let norm2: f32 = v.iter().map(|x| x * x).sum();
    match metric {
        Metric::DotProduct => Some(v.to_vec()),
        Metric::CosineDistance => (norm2 > 0.0 && norm2.is_finite()).then(|| {
            let n = norm2.sqrt();
            v.iter().map(|x| x / n).collect()
        }),
        Metric::EuclideanSquared => norm2
            .is_finite()
            .then(|| v.iter().copied().chain([-norm2 / 2.0]).collect()),
    }
}

/// A query as `metric` scores it, or `None` as [`transform_stored`] refuses one (M9d).
fn transform_query(metric: Metric, q: &[f32]) -> Option<Vec<f32>> {
    match metric {
        Metric::EuclideanSquared => q
            .iter()
            .map(|x| x * x)
            .sum::<f32>()
            .is_finite()
            .then(|| q.iter().copied().chain([1.0]).collect()),
        other => transform_stored(other, q),
    }
}

/// The distance a dense score is, under `metric`, for a query of squared norm `q2` (M9d).
/// Clamped to the metric's range, which rounding alone can leave.
fn distance(metric: Metric, score: f32, q2: f32) -> f32 {
    match metric {
        Metric::DotProduct => -score,
        Metric::CosineDistance => (1.0 - score).clamp(0.0, 2.0),
        Metric::EuclideanSquared => (q2 - 2.0 * score).max(0.0),
    }
}

/// Each id's **newest** operation, in the order those operations arrived (M9c.2).
fn newest(ops: Vec<Document>) -> Vec<Document> {
    let mut last: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, d) in ops.iter().enumerate() {
        last.insert(d.id.clone(), i);
    }
    ops.into_iter()
        .enumerate()
        .filter(|(i, d)| last.get(&d.id) == Some(i))
        .map(|(_, d)| d)
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod memtable_tests {
    use super::*;

    fn batch(id: &str) -> BTreeMap<String, Vec<Document>> {
        BTreeMap::from([("i".to_owned(), vec![Document::new(id, vec![1.0])])])
    }

    #[test]
    fn a_prune_drops_exactly_the_batches_a_watermark_folded() {
        let mut m = Memtable {
            durable: vec![(0, batch("a")), (1, batch("b")), (2, batch("c"))],
            ..Memtable::default()
        };
        // Watermark 2: sequences 0 and 1 are folded, 2 is not.
        assert!(m.prune(2));
        let left: Vec<&str> = m.durable_rows("i").map(|d| d.id.as_str()).collect();
        assert_eq!(left, ["c"]);
        assert_eq!(
            m.generation, 1,
            "dropping rows must invalidate a cached fresh segment"
        );
        // The same watermark again changes nothing and is still consistent.
        assert!(m.prune(2));
        assert_eq!(m.generation, 1);
    }

    #[test]
    fn a_head_older_than_a_prune_is_reported_stale() {
        // A query holding a HEAD that folded less than an earlier prune removed would pair it
        // with rows missing what it lacks -- the reverse of row 36. It must re-read HEAD.
        let mut m = Memtable {
            durable: vec![(0, batch("a")), (1, batch("b"))],
            ..Memtable::default()
        };
        assert!(m.prune(1));
        assert!(!m.prune(0), "a HEAD behind the prune was accepted");
        assert!(m.prune(1));
    }
}

/// A fresh segment as one query uses it: its target, rows and store, taken together.
struct FreshView {
    target: Option<pstore_query::Target>,
    rows: Vec<Document>,
    store: Arc<pstore_blob::MemoryStore>,
    shadow: std::collections::HashSet<String>,
    metric: Option<Metric>,
}

impl Fresh {
    fn view(&self) -> FreshView {
        FreshView {
            target: self.target.clone(),
            rows: self.rows.clone(),
            store: Arc::clone(&self.store),
            shadow: self.shadow.clone(),
            metric: self.metric,
        }
    }
}

/// A HEAD older than rows this engine already pruned is re-read, at most this many times.
const STALE_HEAD_RETRIES: usize = 4;

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
    /// The attributes of every hit, in `hits` order: **empty** for a hit on the unfolded
    /// rows, whose attributes [`Engine::resolve_rows`] reads from [`Self::unfolded`].
    ///
    /// ⚠️ From the same blocks that produced [`Self::ids`] (M9a): the block is the unit of
    /// both, so carrying the attributes costs no request and no byte.
    pub attributes: Vec<std::collections::BTreeMap<String, pstore_format::Value>>,
    /// Each hit's distance under the index's metric, in `hits` order, when the query's first
    /// dense leg scored it (M9d): `$dist`. Smaller is nearer under every metric.
    pub dists: Vec<Option<f32>>,
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

/// One resolved hit: its id, fused score, attributes, and `$dist` when the dense leg scored it.
pub type Resolved = (
    String,
    f32,
    std::collections::BTreeMap<String, pstore_format::Value>,
    Option<f32>,
);

/// How a fold ended.
enum Folded {
    /// It committed this epoch.
    Committed(Epoch),
    /// There was nothing to fold; HEAD was at this epoch.
    Nothing(Epoch),
    /// A drop found no such index, and committed nothing (M9f.2).
    Missing,
}

/// What [`Engine::ordered`] returns (M9e).
#[derive(Debug, Clone, PartialEq)]
pub struct Ordered {
    /// The documents, in order, each with its attributes and no vectors.
    pub rows: Vec<Document>,
    /// How many of them are unfolded rows, which another process would not have returned.
    pub unfolded: usize,
    /// Whether the index exists at all -- a segment, or an unfolded operation -- so a caller
    /// can tell "no such index" from "nothing matched" without asking again.
    pub exists: bool,
}

/// What HEAD knows about one index, without reading a single segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStats {
    /// Segments the index is made of.
    pub segments: u64,
    /// Rows across those segments. ⚠️ **Folded rows only** — the freshness layer is a
    /// property of one process and is reported separately, by `pending_indexes`.
    pub documents: u64,
    /// The epoch HEAD was at when this was read.
    pub epoch: Epoch,
    /// What its rows must look like, once a fold has recorded it.
    pub schema: Option<head::IndexSchema>,
    /// The epoch of the last commit that rewrote its segments or their delete vectors -- a
    /// fold into it, a fold deleting from it, a compaction -- or `None` with no segment (M9f).
    pub updated_epoch: Option<Epoch>,
    /// Rows a fold **discarded** because they contradicted that schema. ⚠️ Acknowledged
    /// writes, dropped rather than allowed to stop the tenant, and reported so the discard
    /// is visible rather than silent.
    pub rejected_rows: u64,
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
    /// What the last HEAD this process read said each index's rows must look like.
    ///
    /// ⚠️ **An early refusal, never a source of truth.** Another process may have folded
    /// since, so a stale entry is caught at the flush — which re-reads once per process — and
    /// the fold is the final authority. Populated by every path that already reads HEAD, so
    /// consulting it costs nothing.
    schemas: Mutex<Option<BTreeMap<String, head::IndexSchema>>>,
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
            schemas: Mutex::new(None),
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

    /// Records what a HEAD read said, so the door can refuse without asking again.
    fn remember_schemas(&self, head: &Head) {
        let mut cache = self
            .schemas
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *cache = Some(head.schemas.clone());
    }

    /// Whether this process has never read a HEAD.
    ///
    /// ⚠️ **`None`, not "empty".** A tenant whose indexes have no schema yet reads back an
    /// empty map, and conflating the two made a process pay the schema read on *every* flush
    /// until some fold recorded one — measured as 2 requests per batch where the cost model
    /// says 1. "I have not looked" and "I looked and there was nothing" are different facts.
    fn schemas_unseen(&self) -> bool {
        self.schemas
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none()
    }

    /// The schema this process last saw for `index`, if it has seen HEAD at all.
    fn cached_schema(&self, index: &str) -> Option<head::IndexSchema> {
        self.schemas
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()?
            .get(index)
            .cloned()
    }

    /// The width and text field a batch of rows implies, for **recording** a new schema.
    ///
    /// ⚠️ **The text field is `None` when no row carries one**, because `seal` builds a text
    /// index only for rows that do. Recording the process's knob for an index of pure vectors
    /// would refuse a differently-configured process over a field neither segment has
    /// postings for — a false conflict on the path that hurts most.
    ///
    /// ⚠️ **`first()` is right here and wrong for validation**, which is the distinction code
    /// review found the hard way: a schema being *recorded* takes the width of the rows that
    /// created the index, and a schema being *checked* has to look at every row. The two used
    /// one function, so a wrong-width row that was not first in the batch was sealed into the
    /// segment undetected — a mixed-width segment, which no later guard can refuse, because a
    /// segment declares one width and scores every row against it.
    ///
    /// ⚠️ The metric too (M9d), from the same first row: the schema a fold creates is recorded
    /// BEFORE its reject pass, so the other rows are checked against it.
    fn implied(&self, docs: &[Document]) -> head::IndexSchema {
        let first = docs.iter().find(|d| !is_tombstone(d));
        let has_text = docs.iter().any(|d| self.carries_text(d));
        head::IndexSchema {
            dims: first.map_or(0, |d| d.vector().len() as u32),
            text_field: if has_text {
                self.text_field.clone()
            } else {
                String::new()
            },
            metric: first.map(metric_of).unwrap_or_default(),
        }
    }

    /// Whether this row has text in the attribute this process indexes.
    fn carries_text(&self, doc: &Document) -> bool {
        matches!(
            doc.attrs.get(self.text_field.as_str()),
            Some(pstore_format::Value::Str(s)) if !s.is_empty()
        )
    }

    /// Why this row cannot join this index, if it cannot.
    ///
    /// ⚠️ **Per row.** The width is a property of the row; the text field is a property of the
    /// process, but it only matters for rows that carry text, so a vector-only row written by
    /// a misconfigured process is still perfectly storable.
    fn row_conflict(
        &self,
        index: &str,
        schema: &head::IndexSchema,
        doc: &Document,
    ) -> Option<EngineError> {
        // A tombstone has no vector and no text: nothing about it can contradict a schema.
        if is_tombstone(doc) {
            return None;
        }
        // The metric first (M9d): cosine and dot have one width, so only this tells them apart.
        let metric = metric_of(doc);
        if metric != schema.metric {
            return Some(EngineError::SchemaConflict {
                index: index.to_owned(),
                what: "the distance metric",
                expected: schema.metric.name().to_owned(),
                got: metric.name().to_owned(),
            });
        }
        let dims = doc.vector().len() as u32;
        if dims != schema.dims {
            // In the width the client wrote: the metric's transform is not theirs to know.
            let extra = metric.extra() as u32;
            return Some(EngineError::SchemaConflict {
                index: index.to_owned(),
                what: "the vector width",
                expected: schema.client_dims().to_string(),
                got: dims.saturating_sub(extra).to_string(),
            });
        }
        if !schema.text_field.is_empty()
            && self.text_field != schema.text_field
            && self.carries_text(doc)
        {
            return Some(EngineError::SchemaConflict {
                index: index.to_owned(),
                what: "the text field",
                expected: schema.text_field.clone(),
                got: self.text_field.clone(),
            });
        }
        None
    }

    /// The first row of `docs` that cannot join `index`, as the error to refuse with.
    ///
    /// ⚠️ **Every row, not the first one.** Checking only `docs[0]` let a wrong-width row
    /// anywhere else in a batch through, and a fold merges every lane's rows for an index into
    /// one batch whose order is lane order — so "first" is not even the caller's first.
    fn batch_conflict(
        &self,
        index: &str,
        schema: &head::IndexSchema,
        docs: &[Document],
    ) -> Option<EngineError> {
        docs.iter()
            .find_map(|d| self.row_conflict(index, schema, d))
    }

    fn mem(&self) -> std::sync::MutexGuard<'_, Memtable> {
        self.mem
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records that this engine committed `epoch`.
    ///
    /// ⚠️ **Every commit, not just a fold's.** `gc` and `compact` advance HEAD too, and until
    /// M7e only `fold` wrote this down — so after a reap the engine reported an epoch one
    /// behind the durable one, and so did every `meta.epoch` a query served from that instance
    /// until the next fold. Found by code review, measured through the API: a gc that
    /// committed epoch 6 answered `"epoch": 5`.
    fn record_commit(&self, epoch: Epoch) {
        *self
            .committed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = epoch;
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
    ///
    /// Under `dot_product`, the metric of every index before M9d: see [`Self::write_as`].
    pub async fn write(&self, index: &str, docs: Vec<Document>) -> Result<(), EngineError> {
        self.write_as(index, docs, Metric::DotProduct).await
    }

    /// [`Self::write`], under `metric` (M9d): each dense vector is stored as the metric's
    /// transform, and the row carries the metric to the fold, which records it in the index's
    /// schema. A metric contradicting the index's is refused exactly where a width is.
    ///
    /// # Errors
    /// As [`Self::write`]; and a zero vector under `cosine_distance`, which has no direction,
    /// or an attribute whose name begins `$`, which is reserved.
    pub async fn write_as(
        &self,
        index: &str,
        docs: Vec<Document>,
        metric: Metric,
    ) -> Result<(), EngineError> {
        let mut docs = docs;
        for d in &mut docs {
            if let Some(name) = d.attrs.keys().find(|k| k.starts_with('$')) {
                return Err(EngineError::Format(format!(
                    "document {}: attribute `{name}` is reserved: names beginning `$` are",
                    d.id
                )));
            }
            for field in d.vectors.values_mut() {
                if let pstore_format::VectorField::Dense(vs) = field {
                    for v in vs.iter_mut() {
                        *v = transform_stored(metric, v).ok_or_else(|| {
                            EngineError::Unmeasurable(format!("document {}", d.id))
                        })?;
                    }
                }
            }
            if metric != Metric::DotProduct {
                d.attrs.insert(
                    METRIC_ATTR.to_owned(),
                    pstore_format::Value::Int(metric.code()),
                );
            }
        }
        // ⚠️ Refused at the DOOR, not at the fold. The document model expresses named,
        // plural and sparse fields; the segment layout stores one dense vector until M3b.3.
        // Accepting a document here and discovering at fold time that it cannot be stored
        // means acknowledging a write that will never be visible — and before this check
        // existed, such a document was written as *nothing*, silently.
        for d in &docs {
            pstore_format::check_storable(d).map_err(|e| EngineError::Format(e.to_string()))?;
            // The empty name marks a tombstone (M9c.2); a document carrying it would be a delete.
            if d.attrs.contains_key(TOMBSTONE) {
                return Err(EngineError::Format(format!(
                    "document {}: an attribute with an empty name is reserved",
                    d.id
                )));
            }
        }
        // ⚠️ **A refusal re-reads HEAD once before it stands** (M9f.2): both rungs can be stale
        // after an index is dropped and made again -- the cached schema, and this process's own
        // flushed rows, which a process that only writes never prunes -- and would then refuse
        // the new index's width forever. A read on the refusal path only, never per write, and
        // with the memtable released: it is a std mutex, and an `.await` under it would block.
        {
            let mut m = self.mem();
            if self.door_conflict(&m, index, &docs, metric).is_none() {
                m.buffer(index, docs);
                return Ok(());
            }
        }
        let at = head::read(&*self.store, self.tenant).await?;
        self.remember_schemas(&at.head);
        let mut m = self.mem();
        let _ = m.prune(self.watermark(&at.head));
        if let Some(e) = self.door_conflict(&m, index, &docs, metric) {
            return Err(e);
        }
        m.buffer(index, docs);
        Ok(())
    }

    /// Why `docs` cannot join `index` under `metric`, by what this process knows without a
    /// request: the schema it last read, and the rows it holds unfolded.
    fn door_conflict(
        &self,
        m: &Memtable,
        index: &str,
        docs: &[Document],
        metric: Metric,
    ) -> Option<EngineError> {
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
        // ⚠️ **Rung one of the ladder: the schema this process has already read**, at zero
        // requests. Cold, it has nothing to compare against and the flush is what refuses.
        if let Some(schema) = self.cached_schema(index)
            && let Some(e) = self.batch_conflict(index, &schema, docs)
        {
            return Some(e);
        }
        // ⚠️ **Falls back to the batch's own first row**, which closes the case a reviewer
        // spotted next to the one this ladder was built for: a brand-new index created by a
        // single mixed-width batch had nothing to compare against — no schema yet, no rows
        // yet — so it was accepted, the schema recorded the first row's width, and the others
        // read back at a width they never had. An index's first batch defines its width, so
        // the rest of that batch must agree with it.
        let known = m
            .pending
            .get(index)
            .into_iter()
            .flatten()
            .chain(m.durable_rows(index))
            .find(|d| !is_tombstone(d))
            .or_else(|| docs.first())
            .map(|d| (metric_of(d), d.vector().len()));
        // ⚠️ And one metric (M9d), on the same rung: cosine and dot have one width.
        if let Some((known_metric, _)) = known
            && known_metric != metric
        {
            return Some(EngineError::SchemaConflict {
                index: index.to_owned(),
                what: "the distance metric",
                expected: known_metric.name().to_owned(),
                got: metric.name().to_owned(),
            });
        }
        if let Some((_, expected)) = known
            && let Some(odd) = docs.iter().find(|d| d.vector().len() != expected)
        {
            return Some(EngineError::DimensionMismatch {
                expected: expected.saturating_sub(metric.extra()),
                got: odd.vector().len().saturating_sub(metric.extra()),
            });
        }
        None
    }

    /// Deletes `ids` from `index` (M9c.2): buffered as tombstones, in the same ordered log as
    /// writes, so the newest operation on an id decides it. Flushed and folded as writes are.
    ///
    /// # Errors
    /// None today; the signature matches [`Self::write`]'s.
    pub async fn delete(&self, index: &str, ids: Vec<String>) -> Result<(), EngineError> {
        let ops = ids.into_iter().map(tombstone).collect();
        self.mem().buffer(index, ops);
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
        let mut names: Vec<String> = m
            .pending
            .keys()
            .chain(m.durable.iter().flat_map(|(_, batch)| batch.keys()))
            .cloned()
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// The oldest epoch `as_of` can still answer, from the HEAD it reads. **One read.**
    ///
    /// # Errors
    /// If HEAD cannot be read.
    pub async fn reap_horizon(&self) -> Result<u64, EngineError> {
        Ok(head::read(&*self.store, self.tenant)
            .await?
            .head
            .reaped_before)
    }

    /// Every index HEAD names for this tenant, in name order. **One read, no LIST.**
    ///
    /// # Errors
    /// If HEAD cannot be read. An absent HEAD is an empty list, not an error: a tenant that
    /// has never committed owns no indexes, which is a fact rather than a failure.
    pub async fn indexes(&self) -> Result<Vec<String>, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        // Every path that reads HEAD warms the door's schema cache, so the check costs a
        // request only for a process that has never read one.
        self.remember_schemas(&at.head);
        self.prune_to(&at.head);
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
        self.remember_schemas(&at.head);
        self.prune_to(&at.head);
        Ok(at.head.indexes.get(index).map(|refs| IndexStats {
            // The newest commit that rewrote its segments or their delete vectors (M9f): both
            // keys carry the epoch they were written at, and a retry re-derives them.
            updated_epoch: refs
                .iter()
                .filter_map(|r| head::key_epoch(&r.key))
                .chain(
                    refs.iter()
                        .filter_map(|r| at.head.deletes.get(&r.key))
                        .filter_map(|(k, _)| head::dv_of(k).map(|(_, e)| e)),
                )
                .max()
                .map(Epoch),
            segments: refs.len() as u64,
            // Live rows: a segment's deleted rows are not documents (M9c.2).
            documents: refs
                .iter()
                .map(|r| {
                    let gone = at.head.deletes.get(&r.key).map_or(0, |(_, n)| *n);
                    u64::from(r.rows.saturating_sub(gone))
                })
                .sum(),
            epoch: at.head.epoch,
            schema: at.head.schemas.get(index).cloned(),
            rejected_rows: at.head.schema_rejects.get(index).copied().unwrap_or(0),
        }))
    }

    /// Drops the unfolded rows `head` shows another fold already folded (M9f): what a query
    /// does before pairing rows with a HEAD, done by the paths that only report. A HEAD older
    /// than a prune this engine already did changes nothing.
    fn prune_to(&self, head: &Head) {
        let _ = self.mem().prune(self.watermark(head));
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
        self.resolve_rows(answer)
            .into_iter()
            .map(|(id, score, _, _)| (id, score))
            .collect()
    }

    /// [`Self::resolve`], with each hit's attributes (M9a).
    ///
    /// A hit on the unfolded rows takes its attributes from the row itself; any other from
    /// [`Answer::attributes`], which came with its id.
    #[must_use]
    pub fn resolve_rows(&self, answer: &Answer) -> Vec<Resolved> {
        answer
            .hits
            .iter()
            .enumerate()
            .filter_map(|(i, h)| {
                let row = if h.segment == answer.unfolded_at {
                    answer
                        .unfolded
                        .get(h.row)
                        .map(|d| (d.id.clone(), d.attrs.clone()))
                } else {
                    answer
                        .ids
                        .get(i)
                        .cloned()
                        .flatten()
                        .map(|id| (id, answer.attributes.get(i).cloned().unwrap_or_default()))
                };
                let dist = answer.dists.get(i).copied().flatten();
                row.map(|(id, attrs)| (id, h.score, attrs, dist))
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

    /// Buffers documents **without the schema check**, so a test can build the state the
    /// fold has to survive: rows that contradict the schema, durable in a lane bundle.
    ///
    /// ⚠️ Reachable in production only as a race between two processes that have both never
    /// read HEAD. That is rare enough to be worth an escape hatch here and not worth a
    /// fixture that races two engines to produce it unreliably.
    #[doc(hidden)]
    pub async fn write_without_schema_check_for_test(&self, index: &str, docs: Vec<Document>) {
        self.mem().buffer(index, docs);
    }

    /// Writes everything buffered as **one bundle object**, whatever it covers.
    ///
    /// `RA = 1 W` for the batch, and for every index in it.
    pub async fn flush(&self) -> Result<Option<Seq>, EngineError> {
        self.flush_inner(true).await
    }

    /// Flushes **without** the schema check, for a test that needs contradicting rows to be
    /// durable — the state the fold must survive without stopping the tenant.
    #[doc(hidden)]
    pub async fn flush_without_schema_check_for_test(&self) -> Result<Option<Seq>, EngineError> {
        self.flush_inner(false).await
    }

    async fn flush_inner(&self, check: bool) -> Result<Option<Seq>, EngineError> {
        require_fencing(&*self.store)?;
        let _lane = self.flushing.lock().await;
        // ⚠️ **Rung two, and the rung that makes the fold's drop path a race rather than a
        // routine.** Nothing wrong may become durable, so the schema is read here — **once
        // per process**, on the first flush, beside the lane registration that already reads.
        // A read per flush would be a request per write, which is the cost model this design
        // exists to protect.
        if check && self.schemas_unseen() {
            let at = head::read(&*self.store, self.tenant).await?;
            self.remember_schemas(&at.head);
        }
        // ⚠️ A SNAPSHOT, not a take (M9c.1, row 35): the rows stay in `pending`, visible,
        // while their bundle is written, and move only once it has landed. Writes arriving
        // meanwhile append behind them; flushes are serialized by `flushing`, so the rows this
        // flush wrote are exactly the first `counts[index]` of each index's pending list.
        let pending = {
            let m = self.mem();
            if m.pending.is_empty() {
                return Ok(None);
            }
            m.pending.clone()
        };
        if check {
            let refusal = pending.iter().find_map(|(index, docs)| {
                let schema = self.cached_schema(index)?;
                self.batch_conflict(index, &schema, docs)
            });
            if let Some(e) = refusal {
                return Err(e);
            }
        }
        let seq = *self
            .seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Registered once, on the lane's first flush. A lane nobody can find is a lane whose
        // writes cannot be recovered, and the registration is what makes a successor able to
        // discover it without being told.
        if seq == Seq::ZERO {
            lanes::register(&*self.store, self.tenant, self.lane).await?;
        }
        // The one PUT. ⚠️ **On failure the sequence is not consumed, and this is load-bearing**
        // (OQ-91). A lane is recovered by probing forward from the last watermark until a key is
        // missing, so a lane must be DENSE: the first absent sequence is taken as the end.
        // Burning a number on a failed write punches a permanent hole, and every bundle after
        // it -- all acknowledged, all durable -- becomes invisible to every future reader. Found
        // by the OQ-91 scenario losing two acknowledged rows on seed 0, not by reading this code.
        self.store
            .put(&self.lane_key(seq), bundle::encode(&pending).into())
            .await?;
        {
            let mut s = self
                .seq
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *s = seq.next();
        }
        // ⚠️ No generation bump (M9c.1): the flushed rows move from the front of `pending` to
        // the back of `durable`, and a fresh view is `durable` then `pending` -- the same rows in
        // the same order, so a cached fresh segment built before the move is still exact.
        let mut m = self.mem();
        let mut batch: BTreeMap<String, Vec<Document>> = BTreeMap::new();
        for (idx, docs) in &pending {
            if let Some(rows) = m.pending.get_mut(idx) {
                batch.insert(
                    idx.clone(),
                    rows.drain(..docs.len().min(rows.len())).collect(),
                );
                if rows.is_empty() {
                    m.pending.remove(idx);
                }
            }
        }
        // ⚠️ **A fold may already have folded this bundle** (review of M9c.1): it is in the
        // store before the PUT's future resolves, and `fold` does not take `flushing`, so a
        // fold -- this engine's or another's -- can fold it and prune before this drain. The
        // batch is pushed anyway: the next query's `prune` drops it, and bumps the generation
        // so no cached fresh view keeps it. Until then -- and in the window before this drain,
        // where the rows are in `pending` AND a segment -- a query returns them twice. Residual.
        m.durable.push((seq.0, batch));
        Ok(Some(seq))
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
        match self
            .fold_inner(
                None,
                None::<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>>,
            )
            .await?
        {
            Folded::Committed(epoch) | Folded::Nothing(epoch) => Ok(epoch),
            // Only a drop finds its index missing.
            Folded::Missing => Err(EngineError::Lost),
        }
    }

    /// Deletes `index` (M9f.2): **a fold that drops it**, so every bundle holding its rows up
    /// to each lane's tail is read, and the watermarks advance past them -- no later fold can
    /// bring those rows back. Every other index folds as usual. Returns the epoch that dropped
    /// it, or `None` if it does not exist: HEAD names no segment list or reject count for it,
    /// no bundle read holds a row of it that is not a delete, and neither does this process's
    /// pending memory. `None` commits nothing.
    ///
    /// ⚠️ **Under this lane's flush lock, from before the tails are read until the commit.** An
    /// in-flight flush otherwise lands a bundle past the tail the drop read -- bringing rows
    /// written before the delete back -- and then drains `pending` rows the drop already
    /// removed, marking a later write flushed in a bundle that lacks it (spec review).
    ///
    /// ⚠️ **Linearized at its commit.** A write the drop did not read -- flushed after it read
    /// that lane's tail, or still in another process's memory -- is a write after the delete,
    /// and its fold creates the index again with only that write.
    ///
    /// # Errors
    /// As [`Self::fold`].
    pub async fn delete_index(&self, index: &str) -> Result<Option<Epoch>, EngineError> {
        self.delete_index_inner(
            index,
            None::<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>>,
        )
        .await
    }

    /// [`Self::delete_index`], with `interfere` awaited between the first attempt's reads and
    /// its commit, so a test can make another delete land first and force the retry.
    #[doc(hidden)]
    pub async fn delete_index_with_interference_for_test(
        &self,
        index: &str,
        interfere: impl Future<Output = ()> + Send,
    ) -> Result<Option<Epoch>, EngineError> {
        self.delete_index_inner(index, Some(interfere)).await
    }

    async fn delete_index_inner(
        &self,
        index: &str,
        interfere: Option<impl Future<Output = ()> + Send>,
    ) -> Result<Option<Epoch>, EngineError> {
        let _flushing = self.flushing.lock().await;
        Ok(match self.fold_inner(Some(index), interfere).await? {
            Folded::Committed(epoch) => Some(epoch),
            Folded::Nothing(_) | Folded::Missing => None,
        })
    }

    /// The fold, and -- with `drop` -- the delete of one index (M9f.2).
    async fn fold_inner(
        &self,
        drop: Option<&str>,
        interfere: Option<impl Future<Output = ()> + Send>,
    ) -> Result<Folded, EngineError> {
        require_fencing(&*self.store)?;
        let mut interfere = interfere;
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
            // A drop commits even with nothing else to fold.
            if keys.is_empty() && drop.is_none() {
                return Ok(Folded::Nothing(at.head.epoch));
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
            // ⚠️ **Existence is decided here, on every attempt** (M9f.2): after the bundles are
            // read and before anything is sealed, so a missing index on the first attempt writes
            // nothing. Rows that are all deletes do not make an index exist, as queries decide.
            if let Some(x) = drop {
                let rows = |docs: Option<&Vec<Document>>| {
                    docs.is_some_and(|d| d.iter().any(|d| !is_tombstone(d)))
                };
                // A schema is only ever recorded beside a segment list, so `schemas` adds nothing
                // (mutation sweep); a reject count is NOT -- a fold can count a rejected row of an
                // index whose accepted rows were all deleted, and seal nothing (code review).
                let exists = at.head.indexes.contains_key(x)
                    || at.head.schema_rejects.contains_key(x)
                    || rows(by_index.get(x))
                    || rows(self.mem().pending.get(x));
                if !exists {
                    return Ok(Folded::Missing);
                }
                // Before the reject pass, so none of its rows are counted as rejects.
                by_index.remove(x);
            }
            if by_index.is_empty() && drop.is_none() {
                return Ok(Folded::Nothing(at.head.epoch));
            }
            // ⚠️ Remembered from the HEAD this fold READ, so the door has something to
            // refuse against; the committed one is remembered below, because a fold that
            // records a schema is the moment the process learns it.
            self.remember_schemas(&at.head);

            // ⚠️ **Rung three: drop and count, never stop.** A fold is all-or-nothing across
            // every index in its bundle set and re-reads the same bundles on every attempt, so
            // failing here would stop every later fold for this tenant, forever — one
            // acknowledged row would brick it. Spec review measured that; this is the answer.
            //
            // ⚠️ Before `seal`, so a contradiction costs **zero write-class requests** and
            // cannot orphan an object no HEAD will ever name.
            let mut rejects: BTreeMap<String, u64> = BTreeMap::new();
            // ⚠️ **A new index's schema is implied BEFORE the pass, and the pass runs against it**
            // (M9d, spec review): skipping the pass for a new index sealed two writers' first
            // rows together whatever they disagreed on -- same width, different metrics, and
            // nothing counted.
            let created: BTreeMap<String, head::IndexSchema> = by_index
                .iter()
                .filter(|(idx, _)| !at.head.schemas.contains_key(*idx))
                .map(|(idx, docs)| (idx.clone(), self.implied(docs)))
                .collect();
            for (idx, docs) in &mut by_index {
                let Some(schema) = at.head.schemas.get(idx).or_else(|| created.get(idx)) else {
                    continue;
                };
                // ⚠️ **Per row, not per index.** Dropping the whole index's rows would
                // discard every *correct* row any writer had flushed for it in this span —
                // acknowledged by writers that passed both the door and the flush — and the
                // watermark advances past their bundles, so a later fold never sees them and
                // GC reaps them. Code review measured that: one wrong row cost two innocent
                // ones. The blast radius of a contradiction is the contradicting row.
                let before = docs.len();
                docs.retain(|d| self.row_conflict(idx, schema, d).is_none());
                let dropped = (before - docs.len()) as u64;
                if dropped > 0 {
                    rejects.insert(idx.clone(), dropped);
                }
            }
            // An index whose every row was dropped seals nothing, and must not seal an empty
            // segment either.
            by_index.retain(|_, docs| !docs.is_empty());
            // ⚠️ **The newest operation per id decides it** (M9c.2), AFTER the reject pass: a
            // rejected newest operation leaves the older version standing rather than deleting
            // it. Every id this fold touches supersedes that id in the index's existing
            // segments; only the upserts are sealed.
            let mut touched: BTreeMap<String, std::collections::HashSet<String>> = BTreeMap::new();
            for (idx, docs) in &mut by_index {
                touched.insert(idx.clone(), docs.iter().map(|d| d.id.clone()).collect());
                *docs = newest(std::mem::take(docs))
                    .into_iter()
                    .filter(|d| !is_tombstone(d))
                    .collect();
            }

            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            // The ABA guard: two HEADs differing only in content a content-derived tag is
            // computed from would otherwise share a tag. Derived from the epoch and lane,
            // so it is deterministic and needs no clock.
            next.nonce = nonce_for(next.epoch, self.lane);

            for (idx, ids) in &touched {
                self.supersede(&at.head, &mut next, idx, ids).await?;
            }
            by_index.retain(|_, docs| !docs.is_empty());

            // One segment per index. Folding every index into one object would make each
            // index's ref point at the whole thing, and a scan would return its
            // neighbours' rows.
            for (idx, docs) in &by_index {
                // ⚠️ **The only place a schema is created**, and it records what the rows
                // ARE rather than what this process is configured with: an index of pure
                // vectors records no text field, so a differently-configured process may
                // still fold into it.
                if !next.schemas.contains_key(idx) {
                    next.schemas.insert(idx.clone(), self.implied(docs));
                }
                let seg_key = self.segment_key(next.epoch, idx);
                // Without `$metric` (M9d): the schema holds it now, and a segment never does.
                let sealed: Vec<Document> = docs.iter().cloned().map(stripped).collect();
                self.seal(&seg_key, &sealed, &self.text_field).await?;
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
            // ⚠️ The `> 0` is a **size** guard, not a correctness one: both read sites treat an
            // absent watermark as zero (`unwrap_or(0)` here, `is_some_and(|w| *w > seq.0)` in
            // `head.rs`), so for READS `lane -> 0` is indistinguishable from nothing. What it
            // buys is a HEAD that does not grow a 16-byte entry per idle lane on every fold --
            // and that is observable in the committed HEAD, so `> -> >=` is not equivalent.
            // This comment used to call it provably equivalent; since M8g it is pinned by
            // `a_lane_with_nothing_to_fold_gets_no_watermark`.
            // ⚠️ Counted into the COMMITTED HEAD, because a discard nobody can see is a
            // discard that is indistinguishable from a bug. The API reports it per index.
            for (idx, n) in &rejects {
                *next.schema_rejects.entry(idx.clone()).or_default() += *n;
            }
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
            // The drop (M9f.2): the index's segment list, schema, reject count and delete
            // vectors leave HEAD, the segments and vectors are buried at this epoch -- written
            // even when empty, so the GC that prunes `dropped` is never skipped for having
            // nothing due -- and the schema is kept in `dropped` for `as_of`.
            if let Some(x) = drop {
                let grave = next.graveyard.entry(next.epoch.0).or_default();
                for r in next.indexes.remove(x).unwrap_or_default() {
                    if let Some((dv, _)) = next.deletes.remove(&r.key) {
                        grave.push(dv);
                    }
                    grave.push(r.key);
                }
                next.schema_rejects.remove(x);
                if let Some(schema) = next.schemas.remove(x) {
                    next.dropped.push((x.to_owned(), next.epoch.0, schema));
                }
            }
            if let Some(f) = interfere.take() {
                f.await;
            }

            match head::commit(&*self.store, self.tenant, &at, &next).await {
                Ok(epoch) => {
                    // A drop's pending rows go with it, and no cached fresh view may keep
                    // serving them. Its durable batches are all below the new watermark (the
                    // flush lock is held), so the prune below removes them.
                    if let Some(x) = drop {
                        let mut m = self.mem();
                        m.pending.remove(x);
                        m.generation += 1;
                    }
                    // A fold that recorded a schema is the moment this process learns it, so
                    // the door refuses against the committed state rather than the one read
                    // before the fold.
                    self.remember_schemas(&next);
                    // ⚠️ Only what this fold folded (M9c.1, row 36): a batch flushed after the
                    // lane was probed has a sequence at or past the new watermark and stays.
                    self.mem().prune(self.watermark(&next));
                    self.record_commit(epoch);
                    return Ok(Folded::Committed(epoch));
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

    /// Marks every row of `index`'s existing segments whose id is in `ids` as deleted, writing
    /// each affected segment a new cumulative delete vector and recording it in `next` (M9c.2).
    ///
    /// ⚠️ **Reads the ids of every existing segment of the index** -- one open and one block
    /// read each, in parallel -- because ids cannot be pruned by a zone map. That is the price of
    /// an update that is a write rather than a read-modify-write; compaction bounds the segment
    /// count, not the bytes, and the fold's cost reports it.
    async fn supersede(
        &self,
        head: &Head,
        next: &mut Head,
        index: &str,
        ids: &std::collections::HashSet<String>,
    ) -> Result<(), EngineError> {
        let refs = head.indexes.get(index).cloned().unwrap_or_default();
        let found = futures_util::future::try_join_all(refs.iter().map(|r| async move {
            let key = Key::new(r.key.clone());
            let old = head.deletes.get(&r.key).map(|(k, _)| k.clone());
            let (seg, before) =
                futures_util::future::join(Segment::open(&*self.store, &key), async {
                    match &old {
                        Some(k) => self
                            .store
                            .get(&Key::new(k.clone()))
                            .await
                            .map(|raw| pstore_query::deletes::decode(&raw)),
                        None => Ok(std::collections::HashSet::new()),
                    }
                })
                .await;
            let (seg, mut rows) = (seg?, before?);
            let hit: Vec<usize> = seg
                .rows_where(&*self.store, &key, |_| true)
                .await?
                .into_iter()
                .filter(|(row, d)| ids.contains(&d.id) && !rows.contains(row))
                .map(|(row, _)| row)
                .collect();
            if hit.is_empty() {
                return Ok::<_, EngineError>(None);
            }
            rows.extend(hit);
            Ok(Some((r.key.clone(), old, rows)))
        }))
        .await?;
        for (segment, old, rows) in found.into_iter().flatten() {
            let key = head::dv_key(&segment, next.epoch.0, self.lane.0);
            self.store
                .put(
                    &Key::new(key.clone()),
                    bytes::Bytes::from(pstore_query::deletes::encode(&rows)),
                )
                .await?;
            if let Some(old) = old {
                next.graveyard.entry(next.epoch.0).or_default().push(old);
            }
            next.deletes.insert(segment, (key, rows.len() as u32));
        }
        Ok(())
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
                // And every delete vector HEAD names (M9c.2).
                .chain(at.head.deletes.values().map(|(k, _)| k.as_str()))
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
            next.record_reap(horizon);
            for e in &due {
                next.graveyard.remove(e);
            }
            match head::commit(&*self.store, self.tenant, &at, &next).await {
                Ok(epoch) => {
                    self.record_commit(epoch);
                    return Ok(doomed.len());
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
        self.compact_inner(
            index,
            None::<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>>,
        )
        .await
    }

    /// `compact`, with `interfere` awaited **between the seal and the first commit attempt**,
    /// so a test can force the retry that used to leave a key stamped with the wrong epoch.
    ///
    /// ⚠️ Interfering *before* `compact` proves nothing: the compaction would simply read the
    /// newer HEAD and derive the right key first time. The window that mattered is the one
    /// between writing the object and conditioning on the world.
    #[doc(hidden)]
    pub async fn compact_with_interference_for_test(
        &self,
        index: &str,
        interfere: impl Future<Output = ()> + Send,
    ) -> Result<Option<Epoch>, EngineError> {
        self.compact_inner(index, Some(interfere)).await
    }

    /// A segment's rows `filter` admits, less the rows its delete vector names, in row order
    /// (M9c.2). A segment without a vector is a plain scan; with one, the same zone pruning
    /// through `rows_where`, which also says which row each document is.
    async fn live_rows(
        &self,
        seg: &Segment,
        key: &Key,
        vector: Option<&(String, u32)>,
        filter: Option<&Filter>,
    ) -> Result<Vec<Document>, EngineError> {
        let Some((dv, _)) = vector else {
            return Ok(seg.scan(&*self.store, key, filter).await?);
        };
        let (raw, rows) = futures_util::future::join(
            self.store.get(&Key::new(dv.clone())),
            seg.rows_where(&*self.store, key, |zones| {
                filter.is_none_or(|f| {
                    // The legacy filter matches structurally: only int rows, which the int
                    // zone covers whole.
                    zones
                        .ints
                        .get(f.column())
                        .is_none_or(|(lo, hi)| f.could_match(*lo, *hi))
                })
            }),
        )
        .await;
        let deleted = pstore_query::deletes::decode(&raw?);
        Ok(rows?
            .into_iter()
            .filter(|(row, d)| !deleted.contains(row) && filter.is_none_or(|f| f.matches(d)))
            .map(|(_, d)| d)
            .collect())
    }

    async fn compact_inner(
        &self,
        index: &str,
        interfere: Option<impl Future<Output = ()> + Send>,
    ) -> Result<Option<Epoch>, EngineError> {
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
        // ⚠️ **The inputs' delete vectors as this HEAD names them** (M9c.2): the merge drops the
        // rows they name, and is abandoned below if a fold changes any of them first -- a merge
        // sealed before a delete and committed after it would resurrect the deleted row.
        let vectors: Vec<Option<(String, u32)>> = inputs
            .iter()
            .map(|i| at.head.deletes.get(&i.key).cloned())
            .collect();
        let scanned = futures_util::future::try_join_all(
            opened
                .iter()
                .zip(&keys)
                .zip(&vectors)
                .map(|((seg, k), dv)| self.live_rows(seg, k, dv.as_ref(), None)),
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

        let mut out_key = self.compacted_key(at.head.epoch.next(), index);
        // The single W (two, for an index with a sparse field). Written BEFORE the commit.
        // ⚠️ None when every input row is deleted (M9c.2): the merge then only removes.
        let empty = rows.is_empty();
        if !empty {
            self.seal(&out_key, &rows, text_field).await?;
        }
        // ⚠️ **Every key this attempt and its retries have written**, so a stale one can be
        // buried rather than left for M6e's orphan sweeper.
        let mut stale: Vec<Key> = Vec::new();
        if let Some(f) = interfere {
            f.await;
        }

        let mut at = at;
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            // ⚠️ **The key is re-derived whenever a retry moves the epoch, and the bytes are
            // re-PUT at it.** A key used to be derived once and kept, so a compaction that lost
            // a CAS wrote `N+1` and committed it at `N+2` — and the manifest of `N+1` then
            // reconstructed as the merged segment AND both its inputs, because a segment's key
            // epoch is what says when it became live. One extra PUT on a contended compaction,
            // no rebuild, and the invariant every past epoch depends on holds by construction.
            let want = self.compacted_key(at.head.epoch.next(), index);
            if want != out_key && !empty {
                self.seal(&want, &rows, text_field).await?;
                stale.push(out_key.clone());
                out_key = want;
            }
            let out = SegmentRef {
                key: out_key.as_str().to_owned(),
                rows: rows.len() as u32,
            };
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
            // ⚠️ And if a fold deleted rows of an input since the read (M9c.2, review B1): the
            // merged rows include them, and its vector would be buried with the input.
            if inputs
                .iter()
                .zip(&vectors)
                .any(|(i, dv)| at.head.deletes.get(&i.key) != dv.as_ref())
            {
                return Ok(None);
            }

            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            next.nonce = nonce_for(next.epoch, self.lane);
            // The inputs' delete vectors die with them: the merge already dropped their rows.
            for i in &inputs {
                if let Some((dv, _)) = next.deletes.remove(&i.key) {
                    next.graveyard.entry(next.epoch.0).or_default().push(dv);
                }
            }
            // Segments added since we read: kept, in place, after the merged one. Dropping
            // them would silently discard every row folded while we were merging.
            let mut kept: Vec<SegmentRef> = if empty { Vec::new() } else { vec![out.clone()] };
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
            // ⚠️ **Buried under ITS OWN key epoch, not this one.** The graveyard means "was
            // live, and stopped being referenced here"; a key that was never live has no such
            // epoch, and burying it at the committing one would put it straight back into the
            // arm that reconstructs a past manifest — the merge and its inputs, together.
            for k in &stale {
                let born = head::key_epoch(k.as_str()).unwrap_or(next.epoch.0);
                next.graveyard
                    .entry(born)
                    .or_default()
                    .push(k.as_str().to_owned());
            }

            match head::commit(&*self.store, self.tenant, &at, &next).await {
                Ok(epoch) => {
                    self.record_commit(epoch);
                    return Ok(Some(epoch));
                }
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
        // ⚠️ The unfolded rows must be the ones THIS HEAD has not folded (M9c.1): a HEAD that
        // is older than a prune this engine already did is re-read rather than paired with
        // rows that no longer include what it lacks.
        for _ in 0..STALE_HEAD_RETRIES {
            let at = head::read(&*self.store, self.tenant).await?;
            let refs: Vec<&SegmentRef> = at.head.indexes.get(index).into_iter().flatten().collect();
            let keys: Vec<Key> = refs.iter().map(|r| Key::new(r.key.clone())).collect();

            let opened = futures_util::future::try_join_all(
                keys.iter().map(|k| Segment::open(&*self.store, k)),
            )
            .await?;
            let scanned =
                futures_util::future::try_join_all(opened.iter().zip(&keys).zip(&refs).map(
                    |((seg, k), r)| self.live_rows(seg, k, at.head.deletes.get(&r.key), filter),
                ))
                .await?;
            let mut out: Vec<Document> = scanned.into_iter().flatten().collect();
            let mut m = self.mem();
            if !m.prune(self.watermark(&at.head)) {
                continue;
            }
            let unfolded: Vec<Document> = m
                .durable_rows(index)
                .chain(m.pending.get(index).into_iter().flatten())
                .cloned()
                .collect();
            drop(m);
            // ⚠️ Each id's newest unfolded operation decides it (M9c.2): its folded rows are
            // shadowed, and a tombstone contributes nothing.
            let shadow: std::collections::HashSet<&str> =
                unfolded.iter().map(|d| d.id.as_str()).collect();
            out.retain(|d| !shadow.contains(d.id.as_str()));
            for d in newest(unfolded) {
                if !is_tombstone(&d) && filter.is_none_or(|f| f.matches(&d)) {
                    out.push(stripped(d));
                }
            }
            return Ok(out);
        }
        Err(EngineError::Lost)
    }

    /// Seals this index's unfolded rows into a segment that lives only in memory.
    ///
    /// ⚠️ Returns `None` when nothing is unfolded — an **empty** fresh segment would occupy an
    /// ordinal for nothing, and every other ordinal is a number a caller resolves against HEAD.
    ///
    /// ⚠️ A refusal here **fails the query**. Falling back to the folded-only answer would be
    /// silently returning the stale result this exists to remove, which is worse than an error
    /// a caller can see.
    async fn fresh_view(
        &self,
        index: &str,
        watermark: u64,
    ) -> Result<Option<Option<FreshView>>, EngineError> {
        let (generation, ops) = {
            let mut m = self.mem();
            if !m.prune(watermark) {
                return Ok(None);
            }
            let ops: Vec<Document> = m
                .durable_rows(index)
                .chain(m.pending.get(index).into_iter().flatten())
                .cloned()
                .collect();
            (m.generation, ops)
        };
        let mut slot = self.fresh.lock().await;
        // ⚠️ The view is returned from UNDER this lock (M9c.1, row 37): re-locking to read the
        // rows and store afterwards let a concurrent query on another index replace the cached
        // segment in between, and this one then ran `mem/a.seg` against `b`'s store.
        if let Some(f) = slot.as_ref()
            && f.generation == generation
            && f.index == index
        {
            return Ok(Some(Some(f.view())));
        }
        if ops.is_empty() {
            *slot = None;
            return Ok(Some(None));
        }
        // ⚠️ The newest operation per id decides it (M9c.2): a later write replaces an earlier
        // one, a tombstone removes it -- and every id touched shadows its older rows in the
        // index's segments, whether its newest operation was a write or a delete.
        let shadow: std::collections::HashSet<String> = ops.iter().map(|d| d.id.clone()).collect();
        let rows: Vec<Document> = newest(ops)
            .into_iter()
            .filter(|d| !is_tombstone(d))
            .collect();
        // The metric read, then stripped (M9d): no segment, fresh ones included, stores it.
        let metric = rows.first().map(metric_of);
        let rows: Vec<Document> = rows.into_iter().map(stripped).collect();
        if rows.is_empty() {
            let fresh = Fresh {
                generation,
                index: index.to_owned(),
                store: Arc::new(pstore_blob::MemoryStore::new()),
                target: None,
                rows,
                shadow,
                metric,
            };
            let view = fresh.view();
            *slot = Some(fresh);
            return Ok(Some(Some(view)));
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
            deleted: None,
            shadowed: false,
        };
        // In the segment's row order, which the clustering decides — not the order written.
        let ordered: Vec<Document> = built
            .order
            .iter()
            .filter_map(|r| rows.get(*r).cloned())
            .collect();
        let fresh = Fresh {
            generation,
            index: index.to_owned(),
            store,
            target: Some(target),
            rows: ordered,
            shadow,
            metric,
        };
        let view = fresh.view();
        *slot = Some(fresh);
        Ok(Some(Some(view)))
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
        self.query_filtered(index, prefetch, None, fusion, top_k)
            .await
    }

    /// [`Self::query`], answering only with documents `filter` admits (M9b) — the unfolded
    /// rows included, which are the fresh segment and take the same path.
    ///
    /// # Errors
    /// As [`Self::query`].
    pub async fn query_filtered(
        &self,
        index: &str,
        prefetch: &[pstore_query::Prefetch],
        filter: Option<&pstore_query::Predicate>,
        fusion: pstore_query::Fusion,
        top_k: usize,
    ) -> Result<Answer, EngineError> {
        // ⚠️ HEAD and the unfolded rows must agree on what is folded (M9c.1): a HEAD older than
        // a prune this engine already did would be paired with rows missing what it lacks.
        let (at, fresh) = self.head_and_fresh(index).await?;
        self.remember_schemas(&at.head);
        // ⚠️ Derived, never discovered: a segment's centroid table is at its own key, and
        // absent means "below the exact-scan threshold" rather than missing (D-10).
        let refs: Vec<SegmentRef> = at.head.indexes.get(index).cloned().unwrap_or_default();
        let mut targets = segment_targets(&refs, &at.head.deletes, true);
        let unfolded_at = targets.len();

        // ⚠️ Every id with an unfolded operation hides its older rows in the segments (M9c.2) --
        // even when every such operation was a delete and there is no fresh segment at all.
        let (unfolded, fresh_store, shadow, fresh_metric) = match fresh {
            Some(v) => {
                targets.extend(v.target);
                (v.rows, Some(v.store), v.shadow, v.metric)
            }
            None => (Vec::new(), None, std::collections::HashSet::new(), None),
        };
        // From the HEAD already read, or -- an index not yet folded -- from its unfolded rows.
        let metric = at
            .head
            .schemas
            .get(index)
            .map(|s| s.metric)
            .or(fresh_metric)
            .unwrap_or_default();
        let (prefetch, q2) = scored_by(metric, prefetch)?;
        let prefetch = prefetch.as_slice();
        if targets.is_empty() {
            return Ok(Answer {
                hits: Vec::new(),
                ids: Vec::new(),
                attributes: Vec::new(),
                dists: Vec::new(),
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
        let resolved = pstore_query::query_rows_filtered(
            &store, &targets, prefetch, filter, &shadow, fusion, top_k,
        )
        .await
        .map_err(|e| query_error(metric, e))?;
        let (hits, ids, attributes, dists) = split_rows(resolved, metric, q2);
        Ok(Answer {
            hits,
            ids,
            attributes,
            dists,
            unfolded,
            unfolded_at,
            segments: refs,
        })
    }

    /// HEAD, and the fresh view of `index` consistent with it: the unfolded rows it has not
    /// folded. Re-reads HEAD when this engine already pruned past it.
    async fn head_and_fresh(
        &self,
        index: &str,
    ) -> Result<(head::HeadAt, Option<FreshView>), EngineError> {
        for _ in 0..STALE_HEAD_RETRIES {
            let at = head::read(&*self.store, self.tenant).await?;
            if let Some(view) = self.fresh_view(index, self.watermark(&at.head)).await? {
                return Ok((at, view));
            }
        }
        Err(EngineError::Lost)
    }

    /// How far HEAD has folded this engine's own lane.
    fn watermark(&self, head: &Head) -> u64 {
        head.watermarks.get(&self.lane.0).copied().unwrap_or(0)
    }

    /// The same query, against the manifest as it stood at `epoch`.
    ///
    /// ⚠️ **No freshness layer.** `query` fuses this process's unfolded rows into every answer,
    /// and those rows are newer than any past epoch by definition — an `as_of` that fused them
    /// would be the present wearing a date.
    ///
    /// # Errors
    /// [`EngineError::TimeTravel`] outside the reconstructible window, or as [`Self::query`].
    pub async fn query_as_of(
        &self,
        index: &str,
        epoch: Epoch,
        prefetch: &[pstore_query::Prefetch],
        fusion: pstore_query::Fusion,
        top_k: usize,
    ) -> Result<Answer, EngineError> {
        self.query_as_of_filtered(index, epoch, prefetch, None, fusion, top_k)
            .await
    }

    /// [`Self::query_as_of`] with a filter, as [`Self::query_filtered`].
    ///
    /// # Errors
    /// As [`Self::query_as_of`].
    pub async fn query_as_of_filtered(
        &self,
        index: &str,
        epoch: Epoch,
        prefetch: &[pstore_query::Prefetch],
        filter: Option<&pstore_query::Predicate>,
        fusion: pstore_query::Fusion,
        top_k: usize,
    ) -> Result<Answer, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        self.remember_schemas(&at.head);
        let then = at.head.as_of(epoch)?;
        let refs: Vec<SegmentRef> = then.indexes.get(index).cloned().unwrap_or_default();
        // The delete vectors as they stood at the epoch (`Head::as_of`), and no shadow: the
        // unfolded rows are newer than any past epoch.
        let targets = segment_targets(&refs, &then.deletes, false);
        if targets.is_empty() {
            return Ok(Answer {
                hits: Vec::new(),
                ids: Vec::new(),
                attributes: Vec::new(),
                dists: Vec::new(),
                unfolded: Vec::new(),
                unfolded_at: 0,
                segments: refs,
            });
        }
        // ⚠️ **One past the last segment, never zero.** `unfolded_at` is the ordinal the
        // freshness layer would occupy, and `resolve` reads a hit carrying it as "look in the
        // unfolded rows". A past answer has no unfolded rows — but segment **0** is a real
        // segment, so setting it to zero made every hit in the first segment resolve against
        // an empty list and vanish. The test found it; the live path has always used this
        // value for the same reason.
        let unfolded_at = refs.len();
        // A schema is immutable while its index lives (M9d); across a drop, the dropped one
        // (M9f.2).
        let metric = metric_at(&at.head, index, epoch);
        let (prefetch, q2) = scored_by(metric, prefetch)?;
        let resolved = pstore_query::query_rows_filtered(
            &*self.store,
            &targets,
            &prefetch,
            filter,
            &std::collections::HashSet::new(),
            fusion,
            top_k,
        )
        .await
        .map_err(|e| query_error(metric, e))?;
        let (hits, ids, attributes, dists) = split_rows(resolved, metric, q2);
        Ok(Answer {
            hits,
            ids,
            attributes,
            dists,
            unfolded: Vec::new(),
            unfolded_at,
            segments: refs,
        })
    }

    /// Every document `filter` admits, in `by`'s order, from `offset` for `limit` (M9e).
    ///
    /// **Three round trips** -- HEAD, the segments opened with their delete vectors, their
    /// admitted blocks -- and no resolve round: a block carries ids and attributes. Rows are
    /// **selected** into `offset + limit`, never sorted whole, so an unfiltered order over a
    /// large index holds its fetched bytes and not its decoded rows. With `as_of`, that epoch's
    /// manifest and delete vectors, and no unfolded rows.
    ///
    /// # Errors
    /// [`EngineError::TimeTravel`] outside the window; a segment or block that cannot be read.
    pub async fn ordered(
        &self,
        index: &str,
        by: &pstore_query::OrderBy,
        filter: Option<&pstore_query::Predicate>,
        offset: usize,
        limit: usize,
        as_of: Option<Epoch>,
    ) -> Result<Ordered, EngineError> {
        let (refs, targets, unfolded, shadow) = match as_of {
            Some(epoch) => {
                let at = head::read(&*self.store, self.tenant).await?;
                self.remember_schemas(&at.head);
                let then = at.head.as_of(epoch)?;
                let refs = then.indexes.get(index).cloned().unwrap_or_default();
                let targets = segment_targets(&refs, &then.deletes, false);
                (refs, targets, Vec::new(), std::collections::HashSet::new())
            }
            None => {
                let (at, fresh) = self.head_and_fresh(index).await?;
                self.remember_schemas(&at.head);
                let refs = at.head.indexes.get(index).cloned().unwrap_or_default();
                let targets = segment_targets(&refs, &at.head.deletes, true);
                let (rows, shadow) = fresh.map_or_else(
                    || (Vec::new(), std::collections::HashSet::new()),
                    |v| (v.rows, v.shadow),
                );
                (refs, targets, rows, shadow)
            }
        };
        let mut selector = pstore_query::Selector::new(by.clone(), offset.saturating_add(limit));
        pstore_query::select(&*self.store, &targets, filter, &shadow, &mut selector)
            .await
            .map_err(|e| EngineError::Query(e.to_string()))?;
        // As a relevance query decides it: segments, or unfolded rows that are not deletes. An
        // index only unfolded deletes ever touched does not exist (code review).
        let exists = !refs.is_empty() || !unfolded.is_empty();
        // The unfolded rows are the newest version of each id they carry, tombstones already
        // gone; the segments' older rows of those ids were shadowed above.
        for mut d in unfolded {
            if filter.is_none_or(|f| f.admits(&d.id, &d.attrs)) {
                // As a block's rows are: attributes, no vectors.
                d.vectors.clear();
                selector.offer(d);
            }
        }
        let rows: Vec<Document> = selector
            .into_sorted()
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect();
        // A row is unfolded exactly when its id has an unfolded operation: the segment's
        // version of such an id was shadowed.
        let unfolded = rows.iter().filter(|d| shadow.contains(&d.id)).count();
        Ok(Ordered {
            rows,
            unfolded,
            exists,
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
        // ⚠️ The next head's contents are irrelevant: `commit` returns its epoch only on
        // success, and every caller asserts refusal. An `epoch: Epoch(1)` here was an equivalent
        // mutant -- deleting it changed nothing any test could see (M8g).
        head::commit(&*self.store, self.tenant, &stale, &Head::default()).await
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

/// Query targets for HEAD's segments, each with its delete vector (M9c.2).
fn segment_targets(
    refs: &[SegmentRef],
    deletes: &BTreeMap<String, (String, u32)>,
    shadowed: bool,
) -> Vec<pstore_query::Target> {
    refs.iter()
        .map(|r| {
            let segment = Key::new(r.key.clone());
            pstore_query::Target {
                centroids: pstore_index::vec_index::centroid_key(&segment),
                deleted: deletes.get(&r.key).map(|(key, _)| Key::new(key.clone())),
                shadowed,
                segment,
            }
        })
        .collect()
}

/// A resolved ranking, as `Answer`'s three parallel columns: hits, ids, attributes.
#[expect(
    clippy::type_complexity,
    reason = "the three columns `Answer` stores, named there; a struct here would be `Answer` again"
)]
fn split_rows(
    resolved: Vec<(pstore_query::Hit, Option<Document>, Option<f32>)>,
    metric: Metric,
    q2: f32,
) -> (
    Vec<pstore_query::Hit>,
    Vec<Option<String>>,
    Vec<std::collections::BTreeMap<String, pstore_format::Value>>,
    Vec<Option<f32>>,
) {
    let mut hits = Vec::with_capacity(resolved.len());
    let mut ids = Vec::with_capacity(resolved.len());
    let mut attributes = Vec::with_capacity(resolved.len());
    let mut dists = Vec::with_capacity(resolved.len());
    for (h, d, score) in resolved {
        hits.push(h);
        dists.push(score.map(|s| distance(metric, s, q2)));
        match d {
            Some(d) => {
                ids.push(Some(d.id));
                attributes.push(d.attrs);
            }
            None => {
                ids.push(None);
                attributes.push(std::collections::BTreeMap::new());
            }
        }
    }
    (hits, ids, attributes, dists)
}

/// The metric `index` had at `epoch` (M9f.2): the schema of the first drop of that name after
/// the epoch, if it was dropped since -- the present schema is then another index's, or none --
/// and otherwise the present one.
fn metric_at(head: &Head, index: &str, epoch: Epoch) -> Metric {
    head.dropped
        .iter()
        // `>` and `>=` agree: `as_of` the drop's own epoch finds no segment of the index to
        // score, so which schema it would use is never observed (mutation sweep, equivalent).
        .filter(|(name, dropped, _)| name == index && *dropped > epoch.0)
        .min_by_key(|(_, dropped, _)| *dropped)
        .map(|(_, _, schema)| schema.metric)
        .or_else(|| head.schemas.get(index).map(|s| s.metric))
        .unwrap_or_default()
}

/// The legs as `metric` scores them (M9d): each dense query transformed. With the squared
/// norm of the first dense leg's query, which `$dist` needs under `euclidean_squared`.
///
/// ⚠️ **The probe is unchanged, by measurement.** Spec review argued an L2 probe over
/// transformed centroids is swamped by the augmented component; the dot-product probe it
/// proposed measured no better (`a_clustered_segment_recalls_under_every_metric`, M9d's
/// VERIFIED.md), so the one probe every index has stays.
///
/// # Errors
/// A zero dense query under `cosine_distance`: it has no direction to compare.
fn scored_by(
    metric: Metric,
    prefetch: &[pstore_query::Prefetch],
) -> Result<(Vec<pstore_query::Prefetch>, f32), EngineError> {
    let mut q2 = None;
    let legs = prefetch
        .iter()
        .map(|leg| match leg {
            pstore_query::Prefetch::Dense {
                field,
                query,
                limit,
                tune,
            } => {
                q2.get_or_insert_with(|| query.iter().map(|x| x * x).sum::<f32>());
                let query = transform_query(metric, query)
                    .ok_or_else(|| EngineError::Unmeasurable("the query".to_owned()))?;
                Ok(pstore_query::Prefetch::Dense {
                    field: field.clone(),
                    query,
                    limit: *limit,
                    tune: *tune,
                })
            }
            other => Ok(other.clone()),
        })
        .collect::<Result<Vec<_>, EngineError>>()?;
    Ok((legs, q2.unwrap_or(0.0)))
}

/// A query's error as the engine reports it: a width mismatch in the width the CLIENT used,
/// not the stored one the metric's transform widened (M9d).
fn query_error(metric: Metric, e: pstore_query::QueryError) -> EngineError {
    match e {
        pstore_query::QueryError::Format(pstore_format::FormatError::DimensionMismatch {
            expected,
            got,
        }) => EngineError::DimensionMismatch {
            expected: expected.saturating_sub(metric.extra()),
            got: got.saturating_sub(metric.extra()),
        },
        other => EngineError::Query(other.to_string()),
    }
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

    /// ⚠️ The rest of `Split`'s routing, which only `head` had a test for (M7b). Reads route by
    /// key prefix; deletes and listings are the durable store's, because the fresh store is
    /// rebuilt from the memtable and has nothing worth deleting or listing. M8g's sweep found
    /// `get`, `get_suffix`, `delete_batch` and `list_unrestricted` replaceable by constants.
    #[tokio::test]
    async fn the_split_store_routes_reads_by_prefix_and_deletes_and_lists_durably() {
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

        assert_eq!(&split.get(&d_key).await.unwrap()[..], b"durable-object");
        assert_eq!(&split.get(&f_key).await.unwrap()[..], b"fresh");
        assert_eq!(&split.get_suffix(&d_key, 6).await.unwrap()[..], b"object");
        assert_eq!(&split.get_suffix(&f_key, 3).await.unwrap()[..], b"esh");
        assert_eq!(
            split.get_tag(&d_key).await.unwrap(),
            durable.get_tag(&d_key).await.unwrap()
        );
        assert_eq!(
            split.get_tag(&f_key).await.unwrap(),
            fresh.get_tag(&f_key).await.unwrap()
        );
        assert!(split.get_tag(&d_key).await.unwrap().is_some());

        let listed = split
            .list_unrestricted(&Key::new("0007/".to_owned()))
            .await
            .unwrap();
        assert_eq!(
            listed,
            vec![d_key.clone()],
            "the durable store is the one listed"
        );

        split
            .delete_batch(std::slice::from_ref(&d_key))
            .await
            .unwrap();
        assert!(
            durable.get(&d_key).await.is_err(),
            "a delete through the split did not reach the durable store"
        );
    }

    /// ⚠️ A retry that does not wait is the livelock the jitter exists to prevent -- and
    /// `backoff` emptied survived every test (M8g). Under a paused clock the sleep advances
    /// time in whole milliseconds, so the bound is `[delay, delay + 1ms)`, not equality.
    #[tokio::test(start_paused = true)]
    async fn backoff_waits_the_delay_it_computes() {
        let lane = LaneId(3);
        let delay = backoff_delay(lane, 3);
        let start = tokio::time::Instant::now();
        backoff(lane, 3).await;
        let waited = start.elapsed();
        assert!(
            waited >= delay && waited < delay + std::time::Duration::from_millis(1),
            "backoff waited {waited:?} for a delay of {delay:?}"
        );
    }

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
