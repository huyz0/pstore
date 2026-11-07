//! Replication (M22): a replica index follows a source index -- in this tenant, another
//! tenant, or another store -- by pulling the source's folded state and committing it to its
//! own HEAD. See `docs/milestones/M22/SPEC.md`.

use crate::head::{
    self, CopyEnd, CopyFailure, CopyOutcome, CopyProgress, Head, HeadAt, IndexSchema, Reindex,
    ReplicaSource, Replication, fnv1a,
};
use crate::{
    Engine, EngineError, MAX_COMMIT_ATTEMPTS, SegmentRef, backoff, is_rowless, nonce_for,
    require_fencing,
};
use bytes::Bytes;
use futures_util::StreamExt;
use pstore_blob::{BlobError, BlobStore, Key};
use pstore_types::{Epoch, TenantId};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

/// Segments copied at once by one sync, so a worker running one sync at a time holds at most
/// this many in memory (spec § Worker).
const IN_FLIGHT: usize = 4;

/// Consecutive remaps that copied nothing new before a replication fails (spec § Sync 5).
const MAX_STALLS: u32 = 24;

/// Segments a copy's sync copies or rebuilds between two progress checkpoints (M200).
pub(crate) const COPY_CHECKPOINT: usize = 64;

/// Writes one checkpoint tries before it is skipped (M200): it is best-effort.
const CHECKPOINT_TRIES: u32 = 3;

/// A checkpoint point of a copy's sync (M200), as a test hook sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checkpoint {
    /// Before the checkpoint's first write.
    Before {
        /// The copy's dest.
        dest: String,
    },
    /// After it: what it wrote, or `None` when it wrote nothing.
    After {
        /// The copy's dest.
        dest: String,
        /// The progress it committed.
        wrote: Option<CopyProgress>,
    },
}

/// What a [`CheckpointHook`] runs (M200): a future the sync awaits before it goes on.
pub type CheckpointFn =
    dyn Fn(Checkpoint) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync;

/// A test's hook at each [`Checkpoint`] (M200).
#[derive(Clone)]
pub struct CheckpointHook(
    /// Runs at each checkpoint point, and is awaited before the sync goes on.
    pub Arc<CheckpointFn>,
);

impl std::fmt::Debug for CheckpointHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CheckpointHook")
    }
}

/// The sidecars a segment may have, each derived from its key and each optional -- by the
/// same functions that write them and that GC derives them with, so the three never drift.
fn sidecars(segment: &Key) -> [Key; 5] {
    [
        pstore_index::vec_index::centroid_key(segment),
        pstore_format::sparse::dict_key(segment),
        pstore_format::text::dict_key(segment),
        pstore_index::fde_clusters::key(segment),
        pstore_index::fde_clusters::vectors_key(segment),
    ]
}

/// Why a replication rule refused a request (M22).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplicaRefusal {
    /// The destination name is taken: an index, rows, rejects, or a recent drop.
    #[error("the index {0:?} exists: a replication creates its index")]
    IndexExists(String),
    /// The destination already replicates.
    #[error("the index {0:?} already has a replication")]
    Exists(String),
    /// The source index does not exist.
    #[error("the source index {0:?} does not exist")]
    SourceNotFound(String),
    /// No replication by that name.
    #[error("the index {0:?} has no replication")]
    NotFound(String),
    /// A replica takes no writes: it is its source's.
    #[error("the index {0:?} is a replica: it takes no writes until its replication is cancelled")]
    ReadOnly(String),
    /// A replica cannot be dropped while it replicates.
    #[error("the index {0:?} is a replica: cancel its replication before dropping it")]
    Active(String),
    /// A replica answers no past epoch: its copies carry the epoch they were made at.
    #[error("the index {0:?} is a replica: it has no history to travel to")]
    NoHistory(String),
    /// A malformed request.
    #[error("{0}")]
    Invalid(String),
}

/// Where a copy is (M166): the only field a client branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyState {
    /// Its replication runs: no sync has committed it yet.
    Running,
    /// Its replication is paused.
    Paused,
    /// The destination holds the source's folded state.
    Succeeded,
    /// It ended without copying: [`CopyStatus::failure`] says why.
    Failed,
    /// Cancelled before it finished.
    Cancelled,
}

/// A copy's status (M166), from one HEAD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyStatus {
    /// Where it is.
    pub state: CopyState,
    /// The epoch that created (or last resumed) it: the operation's token.
    pub run: u64,
    /// What it copies.
    pub source: ReplicaSource,
    /// The epoch that ended it, once it has.
    pub epoch: Option<u64>,
    /// The source epoch a succeeded copy holds.
    pub source_epoch: Option<u64>,
    /// Why a failed copy failed.
    pub failure: Option<CopyFailure>,
    /// Whether it asked to finish read-only (M167): an export.
    pub read_only: bool,
    /// The options it rebuilds under (M168): a reindex. `None` for a byte copy.
    pub reindex: Option<Reindex>,
    /// The clock of the commit that created it (M194); `None` for a copy made before M194.
    pub created_ms: Option<u64>,
    /// The clock of the commit that ended it (M194); `None` while it runs or is paused.
    pub ended_ms: Option<u64>,
    /// The past source epoch it copies (M196), or `None` for the source's present.
    pub as_of: Option<u64>,
    /// How far a running or paused copy's sync has got (M200), as its last checkpoint wrote
    /// it; `None` before the first and once it has ended.
    pub progress: Option<CopyProgress>,
}

/// What a copy asks besides its source (M196): [`Engine::create_copy_with`]'s options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyOptions {
    /// Finish read-only (M167): an export.
    pub read_only: bool,
    /// Rebuild every segment under these options (M168): a reindex.
    pub reindex: Option<Reindex>,
    /// Copy the source as it stood at this past epoch (M196), not its present.
    pub as_of: Option<u64>,
}

/// What a copy asks besides its source (M166, M167, M168).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Ask {
    read_only: bool,
    reindex: Option<Reindex>,
    as_of: Option<u64>,
}

/// The named remote stores a sync may read (M22): the server's configured sources.
pub trait Sources: Send + Sync {
    /// The store named `name`, or `None` if this process has none by that name.
    fn store(&self, name: &str) -> Option<Arc<dyn BlobStore>>;
}

/// What a worker keeps between syncs of one tenant (M22).
#[derive(Debug, Clone, Default)]
pub struct SyncState {
    /// The running replications, as the dest HEAD last said: `None` until read, and after
    /// [`Self::invalidate`].
    plan: Option<BTreeMap<String, Replication>>,
    /// Which of the plan's replications are copies (M166), read with it.
    once: BTreeSet<String>,
    /// M196: each copy's past epoch, read with it.
    as_of: BTreeMap<String, u64>,
    /// `(dest, run)` to the source fingerprint it last committed or found current.
    known: BTreeMap<(String, u64), u64>,
}

impl SyncState {
    /// M192: each running replication's source, by dest, copies (M166) left out: `None` until
    /// a sync has read the plan.
    #[must_use]
    pub fn sources(&self) -> Option<BTreeMap<String, ReplicaSource>> {
        self.plan.as_ref().map(|p| {
            p.iter()
                .filter(|(d, _)| !self.once.contains(*d))
                .map(|(d, r)| (d.clone(), r.source.clone()))
                .collect()
        })
    }

    /// Forgets the plan, so the next sync re-reads the dest HEAD: for a holder that saw the
    /// register's generation change.
    pub fn invalidate(&mut self) {
        self.plan = None;
    }
}

/// What one sync did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Synced {
    /// Replications whose new state this sync committed.
    pub committed: Vec<String>,
    /// Replications already current: their source matches what they applied.
    pub current: Vec<String>,
    /// Replications that failed this time, and why. The rest are unaffected.
    pub failed: BTreeMap<String, String>,
    /// Nothing changed at any source: the sync read each source's HEAD and stopped.
    pub idle: bool,
    /// Copies this sync ended, succeeded or failed (M166): replications no longer running.
    pub finished: Vec<String>,
}

/// Where a replication's source is read from: this engine's own store, or a remote one.
enum Src<'a, S> {
    Local(&'a S),
    Remote(Arc<dyn BlobStore>),
}

impl<S: BlobStore> Src<'_, S> {
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        match self {
            Self::Local(s) => s.get(key).await,
            Self::Remote(r) => r.get(key).await,
        }
    }
    async fn head(&self, tenant: TenantId) -> Result<HeadAt, EngineError> {
        match self {
            Self::Local(s) => head::read(*s, tenant).await,
            Self::Remote(r) => head::read(&**r, tenant).await,
        }
    }
}

/// A source index's identity at one HEAD: its segment keys, their delete-vector keys and its
/// schema. `None` when the index has no segments there.
fn fingerprint(h: &Head, index: &str) -> Option<u64> {
    let refs = h.indexes.get(index).filter(|r| !r.is_empty())?;
    let mut b = Vec::new();
    for r in refs {
        b.extend_from_slice(r.key.as_bytes());
        b.push(0);
        if let Some((k, _)) = h.deletes.get(&head::dv_ref(index, &r.key)) {
            b.extend_from_slice(k.as_bytes());
        }
        b.push(1);
    }
    if let Some(s) = h.schemas.get(index) {
        b.extend_from_slice(&s.dims.to_le_bytes());
        b.extend_from_slice(s.text_field.as_bytes());
        b.extend_from_slice(&s.metric.code().to_le_bytes());
        b.extend_from_slice(s.fts.encode().as_bytes());
        for t in &s.trigram {
            b.extend_from_slice(t.as_bytes());
            b.push(2);
        }
        // M98: and each named field's width -- nothing when none is recorded, so a source
        // folded before M98 keeps the fingerprint its replicas applied.
        for (n, w) in &s.fields {
            b.extend_from_slice(n.as_bytes());
            b.push(3);
            b.extend_from_slice(&w.to_le_bytes());
        }
    }
    Some(fnv1a(&b))
}

/// The third dash-separated field of a key's last component, before `suffix`: the source
/// hash a replica's segment (`h`) or delete vector (`g`) key carries.
fn carried(key: &str, suffix: &str) -> Option<u64> {
    let stem = key.strip_suffix(suffix)?;
    let tail = match suffix {
        ".dv" => stem.rsplit_once('.')?.1,
        _ => stem.rsplit_once('/')?.1,
    };
    u64::from_str_radix(tail.split('-').nth(2)?, 16).ok()
}

/// What this sync copied, kept across commit attempts and remaps.
#[derive(Debug, Default)]
struct Copies {
    /// `(dest, h)` to the dest segment key holding it.
    segs: BTreeMap<(String, u64), String>,
    /// `(dest, h)` to the segment a reindex rebuilt for it (M168), or `None` when every row
    /// of its source segment was deleted.
    built: BTreeMap<(String, u64), Option<SegmentRef>>,
    /// `(dest segment, g)` to the dest vector key holding it.
    dvs: BTreeMap<(String, u64), String>,
    /// M196: each copied segment's rows (its footer's) and length, and each copied vector's
    /// count, by dest key: what an `as_of` copy's refs say, since its view's do not.
    counts: BTreeMap<String, (u32, u64)>,
    /// Every segment and vector key written, recorded before its PUT, so one that fails
    /// partway is buried too (M19). A segment's sidecars follow it to GC.
    written: Vec<String>,
    /// M200: each copy's progress this attempt, by dest, as its next checkpoint writes it.
    progress: BTreeMap<String, CopyProgress>,
    /// M200: the HEAD the last checkpoint committed, which the publish is planned on.
    checkpointed: Option<HeadAt>,
}

/// A replication's new state, ready to commit.
#[derive(Debug)]
struct Mapped {
    refs: Vec<SegmentRef>,
    /// Per dest segment, the vector it should have.
    dvs: BTreeMap<String, Option<(String, u32)>>,
    schema: Option<IndexSchema>,
    src_epoch: u64,
    fp: u64,
}

/// One segment copy's outcome: its source hash, its dest key, and the sidecars it lacked.
type Copied = (u64, String, Result<(bool, u32, u64), CopyErr>);

/// Why copying one source segment did not finish.
enum CopyErr {
    /// The source no longer has it: remap against a fresh source HEAD.
    Missing,
    /// Anything else: this replication fails this time.
    Failed(String),
}

/// Why rebuilding one source segment did not finish (M168).
enum BuildErr {
    /// As a copy's.
    Copy(CopyErr),
    /// Its rows cannot be sealed under the reindex's options: the same on every retry, so the
    /// copy ends.
    Refused,
}

impl<E: Into<CopyErr>> From<E> for BuildErr {
    fn from(e: E) -> Self {
        Self::Copy(e.into())
    }
}

/// Why a sync could not map a replication's state (M168): this time, or for good.
enum MirrorErr {
    /// Reported, and retried by the next sync.
    Failed(String),
    /// A reindex's seal was refused: the copy ends `rebuild_refused`.
    Refused,
    /// The past epoch it copies is no longer answerable (M196): the copy ends
    /// `epoch_unavailable`.
    Unavailable,
}

/// The source HEAD a copy reads (M196): `h` itself, or `h` as it stood at `as_of`, with the
/// schema `index` had then (a query `as_of`'s), and none when none was recorded.
fn view(h: Head, index: &str, as_of: Option<u64>) -> Result<Head, head::TimeTravel> {
    let Some(e) = as_of else {
        return Ok(h);
    };
    let mut v = h.as_of(Epoch(e))?;
    match crate::schema_recorded_at(&h, index, Epoch(e)) {
        Some(s) => {
            v.schemas.insert(index.to_owned(), s);
        }
        None => {
            v.schemas.remove(index);
        }
    }
    Ok(v)
}

/// Whether `h` can still answer `epoch`: [`Head::as_of`]'s two refusals, without its copy.
fn answerable(h: &Head, epoch: u64) -> bool {
    epoch <= h.epoch.0 && epoch >= h.reaped_before
}

/// A delete vector as HEAD names it: its key and how many rows it deletes.
type Dv = (String, u32);

/// The hash a reindex's rebuild of a source segment is kept and keyed by (M168): the
/// segment's key and its delete vector's, so a delete on it is a new rebuild, and never a
/// byte copy's `fnv1a(key)`.
fn rebuild_hash(segment: &str, dv: Option<&str>) -> u64 {
    let mut b = segment.as_bytes().to_vec();
    b.push(0);
    b.extend_from_slice(dv.unwrap_or_default().as_bytes());
    fnv1a(&b)
}

impl From<BlobError> for CopyErr {
    fn from(e: BlobError) -> Self {
        match e {
            BlobError::NotFound(_) => Self::Missing,
            e => Self::Failed(e.to_string()),
        }
    }
}

impl<S: BlobStore> Engine<S> {
    /// One CAS on this tenant's HEAD, retried: `change` edits the next HEAD, or answers
    /// `Ok(None)` to write nothing and return the epoch read.
    async fn commit_change(
        &self,
        mut change: impl FnMut(&Head, &mut Head) -> Result<Option<()>, EngineError>,
    ) -> Result<Epoch, EngineError> {
        require_fencing(&*self.store)?;
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let at = head::read(&*self.store, self.tenant).await?;
            self.remember_schemas(&at.head);
            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            next.nonce = nonce_for(next.epoch, self.lane);
            if change(&at.head, &mut next)?.is_none() {
                return Ok(at.head.epoch);
            }
            match self.commit_head(&at, &next).await {
                Ok(epoch) => {
                    self.record_commit(epoch);
                    self.remember_schemas(&next);
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

    /// Creates the replication of `source` into the new index `dest`, running. `from` is the
    /// store `source` names, read once to refuse a source that does not exist.
    ///
    /// # Errors
    /// A [`ReplicaRefusal`] by name, or the store's failure.
    pub async fn create_replication<R: BlobStore + ?Sized>(
        &self,
        dest: &str,
        source: ReplicaSource,
        from: &R,
    ) -> Result<Epoch, EngineError> {
        self.create_inner(dest, source, from, None, &mut None, &mut None)
            .await
    }

    /// [`Self::create_replication`], a copy when `once` is `Some` (M166), an export when it
    /// is `Some(true)` (M167). A copy that the CAS finds already started, as [`same_copy`]
    /// decides, is put in `matched` and nothing is written.
    async fn create_inner<R: BlobStore + ?Sized>(
        &self,
        dest: &str,
        source: ReplicaSource,
        from: &R,
        once: Option<&Ask>,
        matched: &mut Option<CopyStatus>,
        created: &mut Option<u64>,
    ) -> Result<Epoch, EngineError> {
        let refuse = |r| Err(EngineError::Replica(r));
        for n in [dest, source.index.as_str()] {
            if !crate::valid_name(n) {
                return refuse(ReplicaRefusal::Invalid(format!(
                    "the index name {n:?} must match [A-Za-z0-9_.-]{{1,128}} and not be . or .."
                )));
            }
        }
        if source.store.is_empty() && source.tenant == self.tenant && source.index == dest {
            return refuse(ReplicaRefusal::Invalid(
                "an index cannot replicate itself".to_owned(),
            ));
        }
        let as_of = once.and_then(|a| a.as_of);
        if as_of.is_some() && once.is_some_and(|a| a.reindex.is_some()) {
            return refuse(ReplicaRefusal::Invalid(
                "as_of is a copy's or an export's: reindex the export".to_owned(),
            ));
        }
        let there = head::read(from, source.tenant).await?;
        // M196: a past epoch is refused as a query `as_of` it is.
        if as_of.is_some() && there.head.replications.contains_key(&source.index) {
            return refuse(ReplicaRefusal::NoHistory(source.index));
        }
        let there = view(there.head, &source.index, as_of)?;
        if fingerprint(&there, &source.index).is_none() {
            return refuse(ReplicaRefusal::SourceNotFound(source.index));
        }
        // M168: a reindex's names, against the schema it read, before any segment is.
        if let Some(r) = once.and_then(|a| a.reindex.as_ref()) {
            check_reindex(r, there.schemas.get(&source.index))
                .map_err(|why| EngineError::Replica(ReplicaRefusal::Invalid(why)))?;
        }
        self.commit_change(|now, next| {
            if let Some(ask) = once
                && let Some(s) = same_copy(now, dest, &source, ask)
            {
                *matched = Some(s);
                return Ok(None);
            }
            if now.replications.contains_key(dest) {
                return Err(EngineError::Replica(ReplicaRefusal::Exists(
                    dest.to_owned(),
                )));
            }
            // By a branch's rule (M16): an index, rejects, rows, or a drop GC still remembers.
            let rows = {
                let m = self.mem();
                m.pending
                    .get(dest)
                    .into_iter()
                    .flatten()
                    .chain(m.durable_rows(dest))
                    .any(|d| !is_rowless(d))
            };
            if now.indexes.contains_key(dest)
                || now.schema_rejects.contains_key(dest)
                || now.dropped.iter().any(|(n, _, _)| n == dest)
                || rows
            {
                return Err(EngineError::Replica(ReplicaRefusal::IndexExists(
                    dest.to_owned(),
                )));
            }
            self.refuse_past_limit(now, dest)?;
            next.replications.insert(
                dest.to_owned(),
                Replication {
                    source: source.clone(),
                    running: true,
                    run: next.epoch.0,
                    applied: None,
                    rejected: 0,
                },
            );
            // M166: a new operation into `dest` replaces how its last copy ended.
            next.copies.remove(dest);
            // M194: a copy's create records when, as the winning attempt's answer reports it. A
            // replication writes no time: the encode filter drops a stale record for its name.
            if once.is_some() {
                next.once.insert(dest.to_owned());
                let now_ms = self.now_ms();
                next.copy_times.insert(dest.to_owned(), (now_ms, 0));
                *created = Some(now_ms);
                // M196: and the past epoch it copies, or none: never a stale one.
                match as_of {
                    Some(e) => {
                        next.copy_as_of.insert(dest.to_owned(), e);
                    }
                    None => {
                        next.copy_as_of.remove(dest);
                    }
                }
            } else {
                next.once.remove(dest);
            }
            // M167: and whether it asked to finish read-only.
            if once.is_some_and(|a| a.read_only) {
                next.exports.insert(dest.to_owned());
            } else {
                next.exports.remove(dest);
            }
            // M168: and what it rebuilds under.
            match once.and_then(|a| a.reindex.clone()) {
                Some(r) => {
                    next.reindexes.insert(dest.to_owned(), r);
                }
                None => {
                    next.reindexes.remove(dest);
                }
            }
            Ok(Some(()))
        })
        .await
    }

    /// Copies `source` into the new index `dest` (M166): a replication that finishes in the
    /// commit that copies. Answers the copy's status, and whether this call started it: a
    /// running, paused or succeeded copy of `source` into `dest` is answered as it is,
    /// matched before `source` is read.
    ///
    /// # Errors
    /// As [`Self::create_replication`].
    pub async fn create_copy<R: BlobStore + ?Sized>(
        &self,
        dest: &str,
        source: ReplicaSource,
        from: &R,
    ) -> Result<(CopyStatus, bool), EngineError> {
        self.copy_inner(dest, source, from, Ask::default()).await
    }

    /// [`Self::create_copy`] that finishes read-only (M167): an export.
    ///
    /// # Errors
    /// As [`Self::create_copy`].
    pub async fn create_export<R: BlobStore + ?Sized>(
        &self,
        dest: &str,
        source: ReplicaSource,
        from: &R,
    ) -> Result<(CopyStatus, bool), EngineError> {
        let ask = Ask {
            read_only: true,
            ..Ask::default()
        };
        self.copy_inner(dest, source, from, ask).await
    }

    /// [`Self::create_copy`] that rebuilds every segment under `reindex`'s options rather
    /// than copying its bytes (M168): a reindex, read-only at its finish when asked.
    ///
    /// # Errors
    /// As [`Self::create_copy`], and [`ReplicaRefusal::Invalid`] for options that name the
    /// source's primary text field or an attribute it types other than string, or more
    /// further text fields than [`head::MAX_TEXTS`].
    pub async fn create_reindex<R: BlobStore + ?Sized>(
        &self,
        dest: &str,
        source: ReplicaSource,
        from: &R,
        read_only: bool,
        reindex: Reindex,
    ) -> Result<(CopyStatus, bool), EngineError> {
        self.create_copy_with(
            dest,
            source,
            from,
            CopyOptions {
                read_only,
                reindex: Some(reindex),
                as_of: None,
            },
        )
        .await
    }

    /// [`Self::create_copy`] with every option (M196): read-only, a reindex, or a past epoch.
    ///
    /// # Errors
    /// As [`Self::create_reindex`], and, with `as_of`, as a query `as_of` that epoch:
    /// [`EngineError::TimeTravel`] for an epoch the source cannot answer, and
    /// [`ReplicaRefusal::NoHistory`] for a source that is a replica; `as_of` with a reindex is
    /// [`ReplicaRefusal::Invalid`].
    pub async fn create_copy_with<R: BlobStore + ?Sized>(
        &self,
        dest: &str,
        source: ReplicaSource,
        from: &R,
        opts: CopyOptions,
    ) -> Result<(CopyStatus, bool), EngineError> {
        let mut reindex = opts.reindex;
        if let Some(r) = reindex.as_mut() {
            // Sorted, each once, as a schema's set is: two spellings of one set are one ask.
            r.trigram.sort_unstable();
            r.trigram.dedup();
        }
        let ask = Ask {
            read_only: opts.read_only,
            reindex,
            as_of: opts.as_of,
        };
        self.copy_inner(dest, source, from, ask).await
    }

    async fn copy_inner<R: BlobStore + ?Sized>(
        &self,
        dest: &str,
        source: ReplicaSource,
        from: &R,
        ask: Ask,
    ) -> Result<(CopyStatus, bool), EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        if let Some(s) = same_copy(&at.head, dest, &source, &ask) {
            return Ok((s, false));
        }
        let (mut matched, mut created) = (None, None);
        let epoch = self
            .create_inner(
                dest,
                source.clone(),
                from,
                Some(&ask),
                &mut matched,
                &mut created,
            )
            .await?;
        if let Some(s) = matched {
            return Ok((s, false));
        }
        Ok((
            CopyStatus {
                state: CopyState::Running,
                run: epoch.0,
                source,
                epoch: None,
                source_epoch: None,
                failure: None,
                read_only: ask.read_only,
                reindex: ask.reindex,
                created_ms: created,
                ended_ms: None,
                as_of: ask.as_of,
                progress: None,
            },
            true,
        ))
    }

    /// The copy into `dest`'s status (M166), after one HEAD read: `None` when `dest` has no
    /// copy running and no outcome.
    ///
    /// # Errors
    /// If HEAD cannot be read.
    pub async fn copy_status(&self, dest: &str) -> Result<Option<CopyStatus>, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        self.remember_schemas(&at.head);
        Ok(copy_status_in(&at.head, dest))
    }

    /// Sets `dest`'s replication running or paused; a change to running is a new run.
    async fn set_running(&self, dest: &str, running: bool) -> Result<Epoch, EngineError> {
        self.commit_change(|now, next| {
            let Some(r) = now.replications.get(dest) else {
                return Err(EngineError::Replica(ReplicaRefusal::NotFound(
                    dest.to_owned(),
                )));
            };
            if r.running == running {
                return Ok(None);
            }
            let epoch = next.epoch.0;
            if let Some(r) = next.replications.get_mut(dest) {
                r.running = running;
                if running {
                    r.run = epoch;
                }
            }
            Ok(Some(()))
        })
        .await
    }

    /// Pauses `dest`'s replication. Idempotent: pausing a paused one writes nothing.
    ///
    /// # Errors
    /// [`ReplicaRefusal::NotFound`], or the store's failure.
    pub async fn pause_replication(&self, dest: &str) -> Result<Epoch, EngineError> {
        self.set_running(dest, false).await
    }

    /// Resumes `dest`'s replication as a new run. Idempotent.
    ///
    /// # Errors
    /// [`ReplicaRefusal::NotFound`], or the store's failure.
    pub async fn resume_replication(&self, dest: &str) -> Result<Epoch, EngineError> {
        self.set_running(dest, true).await
    }

    /// Cancels `dest`'s replication: `dest` stays, as a normal index whose history starts at
    /// the epoch returned (M16's `branched`).
    ///
    /// # Errors
    /// [`ReplicaRefusal::NotFound`], or the store's failure.
    pub async fn cancel_replication(&self, dest: &str) -> Result<Epoch, EngineError> {
        self.commit_change(|now, next| {
            if !now.replications.contains_key(dest) {
                return Err(EngineError::Replica(ReplicaRefusal::NotFound(
                    dest.to_owned(),
                )));
            }
            next.replications.remove(dest);
            next.branched.insert(dest.to_owned(), next.epoch.0);
            // M166: a copy cancelled says so.
            if next.once.remove(dest)
                && let Some(r) = now.replications.get(dest)
            {
                next.record_copy(
                    dest,
                    CopyOutcome {
                        source: r.source.clone(),
                        run: r.run,
                        epoch: next.epoch.0,
                        end: CopyEnd::Cancelled,
                    },
                );
                self.stamp_end(next, dest);
            }
            Ok(Some(()))
        })
        .await
    }

    /// This tenant's replications, after one HEAD read.
    ///
    /// # Errors
    /// If HEAD cannot be read.
    pub async fn replications(&self) -> Result<BTreeMap<String, Replication>, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        self.remember_schemas(&at.head);
        Ok(at.head.replications)
    }

    /// `dest`'s replication and its index's stats, after one HEAD read: what a status
    /// reports. `None` when `dest` has no replication.
    ///
    /// # Errors
    /// If HEAD cannot be read.
    pub async fn replication_status(
        &self,
        dest: &str,
    ) -> Result<Option<(Replication, Option<crate::IndexStats>, Option<u64>)>, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        self.remember_schemas(&at.head);
        let Some(r) = at.head.replications.get(dest).cloned() else {
            return Ok(None);
        };
        Ok(Some((
            r,
            crate::stats_of(&at.head, dest),
            at.head.replication_gen(),
        )))
    }

    /// [`Head::replication_gen`] of the current HEAD, after one read.
    ///
    /// # Errors
    /// If HEAD cannot be read.
    pub async fn replication_gen(&self) -> Result<Option<u64>, EngineError> {
        let at = head::read(&*self.store, self.tenant).await?;
        self.remember_schemas(&at.head);
        Ok(at.head.replication_gen())
    }

    /// One sync of every running replication of this tenant. See the spec's § Sync.
    ///
    /// ⚠️ **One at a time per engine**, by its own gate (M192). Two concurrent calls derive the
    /// same copy keys (same lane, same epoch), and one burying a key the other is about to
    /// commit lets a GC reap it -- the lane-key hazard `bury_abandoned` documents for
    /// compaction. A second call waits; two processes have two lanes.
    ///
    /// # Errors
    /// Only what stops every replication: the dest HEAD unreadable, or every commit lost.
    /// A failure of one replication is in [`Synced::failed`].
    pub async fn replicate(
        &self,
        sources: &dyn Sources,
        st: &mut SyncState,
    ) -> Result<Synced, EngineError> {
        self.replicate_inner(
            sources,
            st,
            None::<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>>,
        )
        .await
    }

    /// [`Self::replicate`], with `interfere` awaited after the copies and before the first
    /// commit, so a test can make that commit race something.
    #[doc(hidden)]
    pub async fn replicate_with_interference_for_test(
        &self,
        sources: &dyn Sources,
        st: &mut SyncState,
        interfere: impl Future<Output = ()> + Send,
    ) -> Result<Synced, EngineError> {
        self.replicate_inner(sources, st, Some(interfere)).await
    }

    fn source_of<'a>(&'a self, sources: &dyn Sources, store: &str) -> Option<Src<'a, S>> {
        if store.is_empty() {
            Some(Src::Local(&*self.store))
        } else {
            sources.store(store).map(Src::Remote)
        }
    }

    async fn replicate_inner(
        &self,
        sources: &dyn Sources,
        st: &mut SyncState,
        interfere: Option<impl Future<Output = ()> + Send>,
    ) -> Result<Synced, EngineError> {
        require_fencing(&*self.store)?;
        // M192: held to the end, so a second sync here starts from what this one committed.
        let _one = self.sync_gate.lock().await;
        let mut out = Synced::default();
        // The plan: the running replications, from the dest HEAD when not already known.
        let mut first = None;
        if st.plan.is_none() {
            let at = head::read(&*self.store, self.tenant).await?;
            self.remember_schemas(&at.head);
            let plan: BTreeMap<String, Replication> = at
                .head
                .replications
                .iter()
                .filter(|(_, r)| r.running)
                .map(|(d, r)| (d.clone(), r.clone()))
                .collect();
            st.known
                .retain(|(d, run), _| plan.get(d).is_some_and(|r| r.run == *run));
            st.once = at.head.once.clone();
            st.as_of = at.head.copy_as_of.clone();
            st.plan = Some(plan);
            first = Some(at);
        }
        let plan = st.plan.clone().unwrap_or_default();
        if plan.is_empty() {
            out.idle = true;
            return Ok(out);
        }

        // Step 1: each distinct source HEAD once, together.
        let mut by_source: BTreeMap<(String, TenantId), Vec<&String>> = BTreeMap::new();
        for (d, r) in &plan {
            by_source
                .entry((r.source.store.clone(), r.source.tenant))
                .or_default()
                .push(d);
        }
        let reads = futures_util::future::join_all(by_source.keys().map(|(store, tenant)| {
            let src = self.source_of(sources, store);
            async move {
                match src {
                    None => Err(format!(
                        "unknown_store: this process has no store {store:?}"
                    )),
                    Some(src) => src.head(*tenant).await.map_err(|e| e.to_string()),
                }
            }
        }))
        .await;
        let heads: BTreeMap<(String, TenantId), Result<Head, String>> = by_source
            .keys()
            .cloned()
            .zip(reads.into_iter().map(|r| r.map(|at| at.head)))
            .collect();
        let mut changed: Vec<(String, Head, u64)> = Vec::new();
        // M166: copies whose source holds no folded segment, ended rather than retried.
        // M168: and reindexes whose seal was refused, by the same commit, with their code.
        let mut gone: Vec<(String, Replication, CopyFailure)> = Vec::new();
        for (d, r) in &plan {
            let read = heads
                .get(&(r.source.store.clone(), r.source.tenant))
                .cloned()
                .unwrap_or_else(|| Err("no read of this source".to_owned()));
            // M196: a copy of a past epoch reads its view.
            let as_of = st.as_of.get(d).copied();
            let read = match read {
                Ok(h) => match view(h, &r.source.index, as_of) {
                    Ok(v) => Ok(v),
                    Err(_) => {
                        gone.push((d.clone(), r.clone(), CopyFailure::EpochUnavailable));
                        continue;
                    }
                },
                Err(e) => Err(e),
            };
            match &read {
                Err(e) => {
                    out.failed.insert(d.clone(), e.clone());
                }
                Ok(h) => match fingerprint(h, &r.source.index) {
                    None if st.once.contains(d) => {
                        gone.push((d.clone(), r.clone(), CopyFailure::SourceNotFound));
                    }
                    None => {
                        out.failed.insert(
                            d.clone(),
                            format!("the source index {:?} does not exist", r.source.index),
                        );
                    }
                    Some(fp) if st.known.get(&(d.clone(), r.run)) == Some(&fp) => {
                        out.current.push(d.clone());
                    }
                    Some(fp) => changed.push((d.clone(), h.clone(), fp)),
                },
            }
        }
        if changed.is_empty() && gone.is_empty() {
            out.idle = true;
            return Ok(out);
        }

        // Steps 2-5: copy what each needs, then commit them all in one CAS.
        let mut copies = Copies::default();
        let mut mapped: BTreeMap<String, Mapped> = BTreeMap::new();
        let mut interfere = interfere;
        let mut copy_epoch = None;
        for attempt in 0..MAX_COMMIT_ATTEMPTS {
            let at = match first.take() {
                Some(at) => at,
                None => match head::read(&*self.store, self.tenant).await {
                    Ok(at) => at,
                    Err(e) => {
                        self.bury_abandoned(&copies.written).await;
                        return Err(e);
                    }
                },
            };
            self.remember_schemas(&at.head);
            let e = *copy_epoch.get_or_insert(at.head.epoch.0 + 1);
            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            next.nonce = nonce_for(next.epoch, self.lane);
            let mut included = Vec::new();
            out.current
                .retain(|d| !changed.iter().any(|(c, _, _)| c == d));
            for (d, src_head, fp) in &mut changed {
                if out.failed.contains_key(d.as_str()) || gone.iter().any(|(g, ..)| g == d) {
                    continue;
                }
                // ⚠️ **The replication the plan was made from, or nothing** (code review B1): paused,
                // cancelled, re-created from another source, or resumed since the plan was read,
                // and its source HEAD -- read for the plan's source -- belongs to someone else.
                // Never committed; the plan is re-read next time.
                let planned = plan.get(d.as_str());
                let Some(r) = at.head.replications.get(d.as_str()).filter(|r| {
                    r.running && planned.is_some_and(|p| p.run == r.run && p.source == r.source)
                }) else {
                    st.plan = None;
                    continue;
                };
                if r.applied.is_some_and(|(_, f)| f == *fp) {
                    st.known.insert((d.clone(), r.run), *fp);
                    out.current.push(d.clone());
                    continue;
                }
                // ⚠️ Before any copy (code review M1): a source no newer than what is applied --
                // regressed, or a slower read than a rival's -- would be copied, skipped and
                // buried on every sync, invisibly.
                if !mapped.contains_key(d.as_str())
                    && let Some((applied, _)) = r.applied
                    && src_head.epoch.0 <= applied
                {
                    out.failed.insert(
                        d.clone(),
                        format!(
                            "the source's epoch {} is not after the applied {applied}",
                            src_head.epoch.0
                        ),
                    );
                    continue;
                }
                if !mapped.contains_key(d.as_str()) {
                    let Some(src) = self.source_of(sources, &r.source.store) else {
                        out.failed.insert(d.clone(), "unknown_store".to_owned());
                        continue;
                    };
                    // M168: a reindex rebuilds what a copy would copy. Only a copy holds a
                    // record: every create writes or clears it, so a replication never does.
                    // M200: a copy (not a replication) checkpoints its progress under its run.
                    let run = st.once.contains(d.as_str()).then_some(r.run);
                    let got = match at.head.reindexes.get(d.as_str()) {
                        Some(ask) => {
                            let to = (d.as_str(), run);
                            self.rebuild(&src, &r.source, src_head, to, e, &mut copies, ask)
                                .await
                        }
                        _ => {
                            let as_of = st.as_of.get(d.as_str()).copied();
                            let to = (d.as_str(), &at.head, run);
                            self.mirror(&src, &r.source, src_head, to, e, &mut copies, as_of)
                                .await
                        }
                    };
                    match got {
                        Ok(m) => {
                            mapped.insert(d.clone(), m);
                        }
                        Err(MirrorErr::Failed(why)) => {
                            out.failed.insert(d.clone(), why);
                            continue;
                        }
                        Err(MirrorErr::Refused) => {
                            gone.push((d.clone(), r.clone(), CopyFailure::RebuildRefused));
                            continue;
                        }
                        Err(MirrorErr::Unavailable) => {
                            gone.push((d.clone(), r.clone(), CopyFailure::EpochUnavailable));
                            continue;
                        }
                    }
                }
                let Some(m) = mapped.get(d.as_str()) else {
                    continue;
                };
                // ⚠️ Never backwards (spec § Sync 4): a slower worker's older source state
                // must not replace a newer one a rival already committed.
                if r.applied.is_some_and(|(applied, _)| m.src_epoch <= applied) {
                    continue;
                }
                included.push((d.clone(), r.run));
            }
            // M200: a checkpoint moved HEAD, so this attempt's base is stale. The next one is
            // planned on what the last checkpoint wrote, with no read and nothing copied again.
            if let Some(written) = copies.checkpointed.take() {
                first = Some(written);
                continue;
            }
            let mut used: BTreeSet<String> = BTreeSet::new();
            for (d, _) in &included {
                if let Some(m) = mapped.get(d.as_str()) {
                    self.apply(&mut next, d, m, &mut used);
                }
            }
            // M166: a copy ends in the commit that copies it, or, its source holding nothing,
            // in this one -- only the run the plan was made from, as for a copy above.
            let mut ended = Vec::new();
            for (d, run) in &included {
                if next.once.remove(d.as_str())
                    && let Some(r) = next.replications.remove(d.as_str())
                    && let Some(m) = mapped.get(d.as_str())
                {
                    let epoch = next.epoch.0;
                    next.branched.insert(d.clone(), epoch);
                    // M167: an export is read-only from the commit that finishes it.
                    if next.exports.contains(d.as_str()) {
                        next.read_only.insert(d.clone());
                    }
                    let end = CopyEnd::Succeeded {
                        source_epoch: m.src_epoch,
                    };
                    let outcome = CopyOutcome {
                        source: r.source,
                        run: *run,
                        epoch,
                        end,
                    };
                    next.record_copy(d, outcome);
                    self.stamp_end(&mut next, d);
                    ended.push(d.clone());
                }
            }
            for (d, planned, failure) in &gone {
                let live =
                    at.head.replications.get(d.as_str()).filter(|r| {
                        r.running && r.run == planned.run && r.source == planned.source
                    });
                if live.is_some() && next.once.remove(d.as_str()) {
                    next.replications.remove(d.as_str());
                    let outcome = CopyOutcome {
                        source: planned.source.clone(),
                        run: planned.run,
                        epoch: next.epoch.0,
                        end: CopyEnd::Failed(*failure),
                    };
                    next.record_copy(d, outcome);
                    self.stamp_end(&mut next, d);
                    ended.push(d.clone());
                }
            }
            // M148: each replica this sync writes.
            self.stamp_times(
                &at.head,
                &mut next,
                included.iter().map(|(d, _)| d.as_str()),
            );
            // Copies nothing commits are buried under their own epochs, as a compaction's
            // losers are, so GC reaps them and nothing names them.
            let named: BTreeSet<&str> = next
                .indexes
                .values()
                .flatten()
                .map(|r| r.key.as_str())
                .chain(next.deletes.values().map(|(k, _)| k.as_str()))
                .collect();
            let unused: Vec<String> = copies
                .written
                .iter()
                .filter(|k| !named.contains(k.as_str()))
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            if included.is_empty() && unused.is_empty() && ended.is_empty() {
                return Ok(out);
            }
            for k in &unused {
                let born = head::dv_of(k)
                    .map(|(_, e)| e)
                    .or_else(|| head::key_epoch(k))
                    .unwrap_or(next.epoch.0);
                next.graveyard.entry(born).or_default().push(k.clone());
            }
            if let Some(f) = interfere.take() {
                f.await;
            }
            // M196: a copy of a past epoch checks GC's horizon again before it publishes: in
            // this attempt's own HEAD for a source in it (the commit is conditional on that
            // HEAD, so a later announcement makes it lose), else in a fresh read of the
            // source's. GC announces its horizon before it deletes (M201), so a copy whose
            // GETs met a delete sees it here. One that lapsed ends in the next attempt.
            let mut lapsed = false;
            for (d, _) in &included {
                let (Some(epoch), Some(r)) = (st.as_of.get(d.as_str()), plan.get(d.as_str()))
                else {
                    continue;
                };
                let here = r.source.store.is_empty() && r.source.tenant == self.tenant;
                let ok = if here {
                    Ok(answerable(&at.head, *epoch))
                } else {
                    match self.source_of(sources, &r.source.store) {
                        Some(src) => src
                            .head(r.source.tenant)
                            .await
                            .map(|h| answerable(&h.head, *epoch))
                            .map_err(|e| e.to_string()),
                        None => Err("unknown_store".to_owned()),
                    }
                };
                match ok {
                    Ok(true) => {}
                    Ok(false) => {
                        gone.push((d.clone(), r.clone(), CopyFailure::EpochUnavailable));
                        lapsed = true;
                    }
                    Err(why) => {
                        out.failed.insert(d.clone(), why);
                        lapsed = true;
                    }
                }
            }
            if lapsed {
                continue;
            }
            match self.commit_head(&at, &next).await {
                Ok(epoch) => {
                    self.record_commit(epoch);
                    self.record_reapable(epoch);
                    self.remember_schemas(&next);
                    for (d, run) in included {
                        if let Some(m) = mapped.get(d.as_str()) {
                            st.known.insert((d.clone(), run), m.fp);
                        }
                        out.committed.push(d);
                    }
                    // M166: an ended copy no longer runs, so the next sync re-reads the plan
                    // rather than polling its source.
                    if !ended.is_empty() {
                        st.plan = None;
                    }
                    out.finished = ended;
                    return Ok(out);
                }
                Err(EngineError::Lost | EngineError::Contended)
                    if attempt < MAX_COMMIT_ATTEMPTS - 1 =>
                {
                    backoff(self.lane, attempt).await;
                }
                Err(e) => {
                    self.bury_abandoned(&copies.written).await;
                    return Err(e);
                }
            }
        }
        self.bury_abandoned(&copies.written).await;
        Err(EngineError::Lost)
    }

    /// Writes `m` into `next` as `dest`'s state, burying every dest segment and vector it
    /// replaces. Records what it names in `used`.
    fn apply(&self, next: &mut Head, dest: &str, m: &Mapped, used: &mut BTreeSet<String>) {
        let epoch = next.epoch.0;
        let keep: BTreeSet<&str> = m.refs.iter().map(|r| r.key.as_str()).collect();
        let old = next.indexes.get(dest).cloned().unwrap_or_default();
        let mut grave = Vec::new();
        for o in &old {
            if !keep.contains(o.key.as_str()) {
                grave.push(head::burial(dest, &o.key));
                if let Some((k, _)) = next.deletes.remove(&head::dv_ref(dest, &o.key)) {
                    grave.push(k);
                }
            }
        }
        for (seg, want) in &m.dvs {
            match want {
                Some((k, n)) => {
                    used.insert(k.clone());
                    if let Some((was, _)) = next.deletes.insert(seg.clone(), (k.clone(), *n))
                        && &was != k
                    {
                        grave.push(was);
                    }
                }
                None => {
                    if let Some((was, _)) = next.deletes.remove(seg) {
                        grave.push(was);
                    }
                }
            }
        }
        used.extend(m.refs.iter().map(|r| r.key.clone()));
        next.indexes.insert(dest.to_owned(), m.refs.clone());
        match &m.schema {
            Some(s) => {
                next.schemas.insert(dest.to_owned(), s.clone());
            }
            None => {
                next.schemas.remove(dest);
            }
        }
        if let Some(r) = next.replications.get_mut(dest) {
            r.applied = Some((m.src_epoch, m.fp));
        }
        if !grave.is_empty() {
            next.graveyard.entry(epoch).or_default().extend(grave);
        }
    }

    /// The key a copy of the source segment hashing to `h` gets.
    fn replica_seg_key(&self, dest: &str, epoch: u64, h: u64) -> String {
        format!(
            "{:04x}/tnt/{}/idx/{dest}/seg/R/{epoch:020}-{:016x}-{h:016x}.seg",
            self.tenant.0 as u16, self.tenant.0, self.lane.0
        )
    }

    /// Copies what `dest` lacks of `source` at `src_head`, remapping against a fresh source
    /// HEAD whenever the source moved under it. Answers the state to commit.
    #[allow(clippy::too_many_arguments, reason = "one sync's state, threaded once")]
    async fn mirror(
        &self,
        src: &Src<'_, S>,
        source: &ReplicaSource,
        src_head: &mut Head,
        (dest, dest_head, run): (&str, &Head, Option<u64>),
        e: u64,
        copies: &mut Copies,
        as_of: Option<u64>,
    ) -> Result<Mapped, MirrorErr> {
        let index = source.index.as_str();
        let mut stalls = 0;
        loop {
            let Some(fp) = fingerprint(src_head, index) else {
                return Err(MirrorErr::Failed(format!(
                    "the source index {index:?} does not exist"
                )));
            };
            let refs = src_head.indexes.get(index).cloned().unwrap_or_default();
            let have: BTreeMap<u64, String> = dest_head
                .indexes
                .get(dest)
                .into_iter()
                .flatten()
                .filter_map(|r| carried(&r.key, ".seg").map(|h| (h, r.key.clone())))
                .collect();
            let dest_of = |h: u64, copies: &Copies| {
                have.get(&h)
                    .or_else(|| copies.segs.get(&(dest.to_owned(), h)))
                    .cloned()
            };
            let todo: Vec<(String, u64, String)> = refs
                .iter()
                .map(|r| (r.key.clone(), fnv1a(r.key.as_bytes())))
                .filter(|(_, h)| dest_of(*h, copies).is_none())
                .map(|(k, h)| {
                    let to = self.replica_seg_key(dest, e, h);
                    (k, h, to)
                })
                .collect();
            let written = Mutex::new(Vec::new());
            self.planned(copies, dest, run, todo.len());
            // M200: in batches, a checkpoint between two of them.
            let batches: Vec<Vec<(String, u64, String)>> = todo
                .chunks(self.copy_checkpoint)
                .map(<[_]>::to_vec)
                .collect();
            let last = batches.len().saturating_sub(1);
            let mut results: Vec<Copied> = Vec::new();
            for (i, batch) in batches.into_iter().enumerate() {
                let got: Vec<Copied> =
                    futures_util::stream::iter(batch.into_iter().map(|(from, h, to)| {
                        let written = &written;
                        async move {
                            let r = self.copy_segment(src, &from, &to, written).await;
                            (h, to, r)
                        }
                    }))
                    .buffer_unordered(IN_FLIGHT)
                    .collect()
                    .await;
                let done = got.iter().filter_map(|(.., r)| r.as_ref().ok());
                self.tally(copies, dest, done.map(|(_, _, len)| *len));
                if i < last
                    && let Some(run) = run
                {
                    self.checkpoint(dest, run, e, copies).await;
                }
                results.extend(got);
            }
            copies.written.extend(
                written
                    .into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            let mut progress = 0;
            let mut missing = false;
            let mut absent: Vec<u64> = Vec::new();
            for (h, to, r) in results {
                match r {
                    Ok((lacked, rows, len)) => {
                        progress += 1;
                        copies.counts.insert(to.clone(), (rows, len));
                        copies.segs.insert((dest.to_owned(), h), to);
                        if lacked {
                            absent.push(h);
                        }
                    }
                    Err(CopyErr::Missing) => missing = true,
                    Err(CopyErr::Failed(why)) => return Err(MirrorErr::Failed(why)),
                }
            }
            // The delete vectors: copied when the source's key differs from the one the
            // dest's vector was copied from (its `g`), never by count (spec review). Every copy
            // in one round of GETs and PUTs (code review M2).
            let mut dvs = BTreeMap::new();
            let mut mapped = Vec::new();
            let mut fetch: Vec<(String, u64, String, String)> = Vec::new();
            for r in &refs {
                let h = fnv1a(r.key.as_bytes());
                let Some(seg) = dest_of(h, copies) else {
                    continue;
                };
                // M196: what the copied bytes hold, since a view's ref says no rows and no
                // length. A present ref says the same, or no length before M45.
                let (rows, len) = copies
                    .counts
                    .get(&seg)
                    .map_or((r.rows, r.len), |(rows, len)| (*rows, Some(*len)));
                mapped.push(SegmentRef {
                    key: seg.clone(),
                    rows,
                    // M32: the copy's sidecars are the source's.
                    dicts: r.dicts,
                    // M45: and its bytes, so its length.
                    len,
                    // M198: and the fields it searches by clusters.
                    ann: r.ann.clone(),
                });
                let want = src_head.deletes.get(&head::dv_ref(index, &r.key)).cloned();
                let Some((k, n)) = want else {
                    dvs.insert(seg, None);
                    continue;
                };
                let g = fnv1a(k.as_bytes());
                let now = dest_head.deletes.get(&seg).cloned();
                if now.as_ref().and_then(|(d, _)| carried(d, ".dv")) == Some(g) {
                    dvs.insert(seg, now);
                    continue;
                }
                if let Some(d) = copies.dvs.get(&(seg.clone(), g)) {
                    // M196: the count its bytes hold; a view's is not kept.
                    let n = copies.counts.get(d).map_or(n, |(deletes, _)| *deletes);
                    dvs.insert(seg, Some((d.clone(), n)));
                    continue;
                }
                let to = format!("{seg}.{e:020}-{:016x}-{g:016x}.dv", self.lane.0);
                fetch.push((seg, g, k, to));
            }
            copies
                .written
                .extend(fetch.iter().map(|(.., to)| to.clone()));
            let got: Vec<_> =
                futures_util::stream::iter(fetch.into_iter().map(|(seg, g, k, to)| async move {
                    let r = match src.get(&Key::new(k)).await {
                        Ok(bytes) => {
                            // M196: the rows it deletes, for a view whose count is not kept.
                            let deletes = pstore_query::deletes::decode(&bytes).len() as u32;
                            self.store
                                .put(&Key::new(to.clone()), bytes)
                                .await
                                .map(|_| deletes)
                                .map_err(|e| CopyErr::Failed(e.to_string()))
                        }
                        Err(e) => Err(CopyErr::from(e)),
                    };
                    (seg, g, to, r)
                }))
                .buffer_unordered(IN_FLIGHT)
                .collect()
                .await;
            for (seg, g, to, r) in got {
                match r {
                    Ok(deletes) => {
                        progress += 1;
                        copies.counts.insert(to.clone(), (deletes, 0));
                        copies.dvs.insert((seg.clone(), g), to.clone());
                        dvs.insert(seg, Some((to, deletes)));
                    }
                    Err(CopyErr::Missing) => missing = true,
                    Err(CopyErr::Failed(why)) => return Err(MirrorErr::Failed(why)),
                }
            }
            if !missing && absent.is_empty() {
                return Ok(Mapped {
                    refs: mapped,
                    dvs,
                    schema: src_head.schemas.get(index).cloned(),
                    src_epoch: src_head.epoch.0,
                    fp,
                });
            }
            // Something the snapshot named was not there. A fresh source HEAD says whether the
            // source moved (remap) or a sidecar was never written (it stands).
            let fresh = src
                .head(source.tenant)
                .await
                .map_err(|e| MirrorErr::Failed(e.to_string()))?
                .head;
            // M196: a past epoch remaps against the fresh HEAD's view of it.
            let fresh = view(fresh, index, as_of).map_err(|_| MirrorErr::Unavailable)?;
            let still: BTreeSet<u64> = fresh
                .indexes
                .get(index)
                .into_iter()
                .flatten()
                .map(|r| fnv1a(r.key.as_bytes()))
                .collect();
            if !missing && absent.iter().all(|h| still.contains(h)) {
                return Ok(Mapped {
                    refs: mapped,
                    dvs,
                    schema: src_head.schemas.get(index).cloned(),
                    src_epoch: src_head.epoch.0,
                    fp,
                });
            }
            // A copy whose sidecar was missing because the source reaped it is not kept.
            for h in absent.iter().filter(|h| !still.contains(h)) {
                copies.segs.remove(&(dest.to_owned(), *h));
            }
            *src_head = fresh;
            stalls = if progress == 0 { stalls + 1 } else { 0 };
            if stalls >= MAX_STALLS {
                return Err(MirrorErr::Failed(format!(
                    "the source index {index:?} kept changing: {MAX_STALLS} remaps copied nothing"
                )));
            }
        }
    }

    /// [`Self::mirror`] for a reindex (M168): every live row of each source segment at
    /// `src_head`, sealed under `ask` applied to the source's schema, remapping against a
    /// fresh source HEAD whenever something it named was gone.
    #[allow(clippy::too_many_arguments, reason = "one sync's state, threaded once")]
    async fn rebuild(
        &self,
        src: &Src<'_, S>,
        source: &ReplicaSource,
        src_head: &mut Head,
        (dest, run): (&str, Option<u64>),
        e: u64,
        copies: &mut Copies,
        ask: &Reindex,
    ) -> Result<Mapped, MirrorErr> {
        let index = source.index.as_str();
        let mut stalls = 0;
        loop {
            let Some(fp) = fingerprint(src_head, index) else {
                return Err(MirrorErr::Failed(format!(
                    "the source index {index:?} does not exist"
                )));
            };
            // Applied here, to the schema copied, so a type recorded since the POST is kept.
            let was = src_head.schemas.get(index).cloned().unwrap_or_default();
            let schema = ask.apply(&was, &self.text_field);
            // Never a primary: a further field, the source's or the reindex's.
            let further: BTreeSet<String> = was
                .texts
                .keys()
                .chain(schema.texts.keys())
                .cloned()
                .collect();
            let refs: Vec<(SegmentRef, Option<Dv>, u64)> = src_head
                .indexes
                .get(index)
                .into_iter()
                .flatten()
                .map(|r| {
                    let dv = src_head.deletes.get(&head::dv_ref(index, &r.key)).cloned();
                    let h = rebuild_hash(&r.key, dv.as_ref().map(|(k, _)| k.as_str()));
                    (r.clone(), dv, h)
                })
                .collect();
            let written = Mutex::new(Vec::new());
            // Owned, as the copy's list is: a stream over borrowed items is not `Send`.
            let todo: Vec<(SegmentRef, Option<Dv>, u64, String)> = refs
                .iter()
                .filter(|(.., h)| !copies.built.contains_key(&(dest.to_owned(), *h)))
                .map(|(r, dv, h)| (r.clone(), dv.clone(), *h, self.replica_seg_key(dest, e, *h)))
                .collect();
            self.planned(copies, dest, run, todo.len());
            // M200: in batches, a checkpoint between two of them, as a copy's.
            type Todo = (SegmentRef, Option<Dv>, u64, String);
            let batches: Vec<Vec<Todo>> = todo
                .chunks(self.copy_checkpoint)
                .map(<[_]>::to_vec)
                .collect();
            let last = batches.len().saturating_sub(1);
            let mut results: Vec<(u64, Result<Option<SegmentRef>, BuildErr>)> = Vec::new();
            for (i, batch) in batches.into_iter().enumerate() {
                let got: Vec<(u64, Result<Option<SegmentRef>, BuildErr>)> =
                    futures_util::stream::iter(batch.into_iter().map(|(r, dv, h, to)| {
                        let (written, schema, further) = (&written, &schema, &further);
                        async move {
                            let out = self
                                .rebuild_segment(
                                    src,
                                    &r,
                                    dv.as_ref(),
                                    &to,
                                    schema,
                                    further,
                                    written,
                                )
                                .await;
                            (h, out)
                        }
                    }))
                    .buffer_unordered(IN_FLIGHT)
                    .collect()
                    .await;
                // A segment whose every row was deleted is rebuilt to nothing: done, 0 bytes.
                let done = got.iter().filter_map(|(_, r)| r.as_ref().ok());
                self.tally(
                    copies,
                    dest,
                    done.map(|b| b.as_ref().and_then(|r| r.len).unwrap_or(0)),
                );
                if i < last
                    && let Some(run) = run
                {
                    self.checkpoint(dest, run, e, copies).await;
                }
                results.extend(got);
            }
            copies.written.extend(
                written
                    .into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            let mut progress: usize = 0;
            let mut missing = false;
            for (h, r) in results {
                match r {
                    Ok(built) => {
                        progress += 1;
                        copies.built.insert((dest.to_owned(), h), built);
                    }
                    Err(BuildErr::Copy(CopyErr::Missing)) => missing = true,
                    Err(BuildErr::Copy(CopyErr::Failed(why))) => {
                        return Err(MirrorErr::Failed(why));
                    }
                    Err(BuildErr::Refused) => return Err(MirrorErr::Refused),
                }
            }
            if !missing {
                let built: Vec<SegmentRef> = refs
                    .iter()
                    .filter_map(|(.., h)| copies.built.get(&(dest.to_owned(), *h)).cloned())
                    .flatten()
                    .collect();
                return Ok(Mapped {
                    dvs: built.iter().map(|r| (r.key.clone(), None)).collect(),
                    refs: built,
                    schema: Some(schema),
                    src_epoch: src_head.epoch.0,
                    fp,
                });
            }
            // Something the snapshot named was gone: the source moved under it.
            *src_head = src
                .head(source.tenant)
                .await
                .map_err(|e| MirrorErr::Failed(e.to_string()))?
                .head;
            stalls = if progress == 0 { stalls + 1 } else { 0 };
            if stalls >= MAX_STALLS {
                return Err(MirrorErr::Failed(format!(
                    "the source index {index:?} kept changing: {MAX_STALLS} remaps copied nothing"
                )));
            }
        }
    }

    /// Rebuilds one source segment (M168): GETs it, its sparse dictionary and its delete
    /// vector through the source's own store in one round, scans it in memory, and seals its
    /// live rows under `schema` at `to`. `None` when every row is deleted. Every name the seal
    /// created, or may have, is put in `written`, whatever the outcome. `further` names every
    /// field that is never the primary.
    #[allow(
        clippy::too_many_arguments,
        reason = "one segment's rebuild, threaded once"
    )]
    async fn rebuild_segment(
        &self,
        src: &Src<'_, S>,
        r: &SegmentRef,
        dv: Option<&(String, u32)>,
        to: &str,
        schema: &IndexSchema,
        further: &BTreeSet<String>,
        written: &Mutex<Vec<String>>,
    ) -> Result<Option<SegmentRef>, BuildErr> {
        let key = Key::new(r.key.clone());
        let dict = pstore_format::sparse::dict_key(&key);
        // `None` dictionaries are a ref from before M32: the sidecar may exist.
        let sparse = r.dicts.is_none_or(|d| d.sparse);
        let (seg, side, deletes) = futures_util::future::join3(
            src.get(&key),
            async {
                if sparse {
                    Some(src.get(&dict).await)
                } else {
                    None
                }
            },
            async {
                match dv {
                    Some((k, _)) => Some(src.get(&Key::new(k.clone())).await),
                    None => None,
                }
            },
        )
        .await;
        let mem = pstore_blob::MemoryStore::new();
        let put = |e: BlobError| CopyErr::Failed(e.to_string());
        let bytes = seg?;
        let len = bytes.len() as u64;
        mem.put(&key, bytes).await.map_err(put)?;
        match side {
            Some(Ok(b)) => {
                mem.put(&dict, b).await.map_err(put)?;
            }
            // A dictionary HEAD records is missing: the source moved.
            Some(Err(BlobError::NotFound(_))) if r.dicts.is_some() => {
                return Err(CopyErr::Missing.into());
            }
            Some(Err(BlobError::NotFound(_))) | None => {}
            Some(Err(e)) => return Err(CopyErr::Failed(e.to_string()).into()),
        }
        let deleted = match deletes {
            Some(raw) => pstore_query::deletes::decode(&raw?),
            None => std::collections::HashSet::new(),
        };
        let failed = |e: pstore_format::FormatError| CopyErr::Failed(e.to_string());
        let opened = pstore_format::Segment::open_at(&mem, &key, Some(len))
            .await
            .map_err(failed)?;
        let rows: Vec<pstore_format::Document> = opened
            .scan(&mem, &key, None)
            .await
            .map_err(failed)?
            .into_iter()
            .enumerate()
            .filter(|(row, _)| !deleted.contains(row))
            .map(|(_, d)| d)
            .collect();
        if rows.is_empty() {
            return Ok(None);
        }
        // The primary the schema records, else the one this segment indexed (M30), as a
        // compaction's: never a further field, the source's or the reindex's -- a dropped
        // further field read as the primary would keep its postings. A recorded primary is
        // never one: M152 keeps it out of the source's, and the create refuses it in the ask.
        let primary = Some(schema.text_field.as_str())
            .filter(|f| !f.is_empty())
            .or_else(|| {
                opened
                    .text_fields()
                    .iter()
                    .map(String::as_str)
                    .find(|f| !further.contains(*f))
            });
        let mut names = crate::Names::default();
        let sealed = self
            .seal(
                &mut names,
                &Key::new(to.to_owned()),
                &rows,
                primary,
                &schema.fts,
                &schema.texts,
                &schema.trigram,
            )
            .await;
        written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(names.written);
        match sealed {
            Ok((key, dicts, len, ann)) => Ok(Some(SegmentRef {
                key: key.as_str().to_owned(),
                rows: rows.len() as u32,
                dicts: Some(dicts),
                len: Some(len),
                ann: Some(ann),
            })),
            Err(EngineError::Format(_)) => Err(BuildErr::Refused),
            Err(e) => Err(CopyErr::Failed(e.to_string()).into()),
        }
    }

    /// Copies one source segment and the sidecars it has: one round of GETs, then one of
    /// PUTs. Answers whether a sidecar was absent, for the caller to confirm against a fresh
    /// source HEAD.
    async fn copy_segment(
        &self,
        src: &Src<'_, S>,
        from: &str,
        to: &str,
        written: &Mutex<Vec<String>>,
    ) -> Result<(bool, u32, u64), CopyErr> {
        let (from, to) = (Key::new(from), Key::new(to));
        let (seg, side) = futures_util::future::join(
            src.get(&from),
            futures_util::future::join_all(
                sidecars(&from).map(|k| async move { src.get(&k).await }),
            ),
        )
        .await;
        let seg = seg?;
        // M196: its rows and length, from the bytes themselves.
        let counts = (
            pstore_format::footer_rows(&seg).unwrap_or_default(),
            seg.len() as u64,
        );
        let mut puts = vec![(to.clone(), seg)];
        let mut absent = false;
        for (r, k) in side.into_iter().zip(sidecars(&to)) {
            match r {
                Ok(b) => puts.push((k, b)),
                Err(BlobError::NotFound(_)) => absent = true,
                Err(e) => return Err(CopyErr::Failed(e.to_string())),
            }
        }
        written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(to.as_str().to_owned());
        // Every PUT awaited, not the first failure: a PUT dropped in flight could still land
        // after its key was buried and reaped (code review round 2).
        let put = futures_util::future::join_all(
            puts.into_iter()
                .map(|(k, b)| async move { self.store.put(&k, b).await }),
        )
        .await;
        if let Some(Err(e)) = put.into_iter().find(Result::is_err) {
            return Err(CopyErr::Failed(e.to_string()));
        }
        Ok((absent, counts.0, counts.1))
    }
}

impl<S: BlobStore> Engine<S> {
    /// Stamps the copy into `dest` as ended by the commit of `next` (M194), at this clock and
    /// never at 0, which reads as not ended. Only a record that exists: a copy made before
    /// M194 has none, and ends with none.
    /// M200: `n` more segments this attempt must copy or rebuild for `dest`, a copy's.
    fn planned(&self, copies: &mut Copies, dest: &str, run: Option<u64>, n: usize) {
        if let Some(run) = run {
            let p = copies.progress.entry(dest.to_owned()).or_default();
            p.run = run;
            p.segments_total = p
                .segments_total
                .saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
        }
    }

    /// M200: segments done for `dest`, each by its bytes.
    fn tally(&self, copies: &mut Copies, dest: &str, done: impl Iterator<Item = u64>) {
        if let Some(p) = copies.progress.get_mut(dest) {
            for len in done {
                p.segments_done = p.segments_done.saturating_add(1).min(p.segments_total);
                p.bytes_done = p.bytes_done.saturating_add(len);
            }
        }
    }

    /// M200: commits `dest`'s progress, best-effort: only while its copy runs under `run`,
    /// at most [`CHECKPOINT_TRIES`] writes, and never an error. What it wrote is the HEAD the
    /// publish is planned on.
    async fn checkpoint(&self, dest: &str, run: u64, attempt: u64, copies: &mut Copies) {
        let Some(p) = copies.progress.get(dest).copied() else {
            return;
        };
        self.hook(Checkpoint::Before {
            dest: dest.to_owned(),
        })
        .await;
        let mut wrote = None;
        for _ in 0..CHECKPOINT_TRIES {
            let Ok(at) = head::read(&*self.store, self.tenant).await else {
                break;
            };
            let live = at.head.once.contains(dest)
                && at
                    .head
                    .replications
                    .get(dest)
                    .is_some_and(|r| r.running && r.run == run);
            if !live {
                break;
            }
            let mut next = at.head.clone();
            next.epoch = next.epoch.next();
            next.nonce = nonce_for(next.epoch, self.lane);
            let p = CopyProgress {
                attempt,
                updated_ms: self.now_ms(),
                ..p
            };
            next.copy_progress.insert(dest.to_owned(), p);
            match self.commit_head_at(&at, &next).await {
                Ok(written) => {
                    self.record_commit(written.head.epoch);
                    copies.checkpointed = Some(written);
                    wrote = Some(p);
                    break;
                }
                Err(EngineError::Lost | EngineError::Contended) => {}
                Err(_) => break,
            }
        }
        self.hook(Checkpoint::After {
            dest: dest.to_owned(),
            wrote,
        })
        .await;
    }

    /// Runs the test's checkpoint hook, if one is set (M200).
    async fn hook(&self, at: Checkpoint) {
        let hook = self
            .checkpoint_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            (hook.0)(at).await;
        }
    }

    fn stamp_end(&self, next: &mut Head, dest: &str) {
        let now_ms = self.now_ms().max(1);
        if let Some((_, ended)) = next.copy_times.get_mut(dest) {
            *ended = now_ms;
        }
    }
}

/// The copy into `dest`'s status in `h` (M166): its running or paused replication, or else
/// how it ended.
fn copy_status_in(h: &Head, dest: &str) -> Option<CopyStatus> {
    if h.once.contains(dest)
        && let Some(r) = h.replications.get(dest)
    {
        return Some(CopyStatus {
            state: if r.running {
                CopyState::Running
            } else {
                CopyState::Paused
            },
            run: r.run,
            source: r.source.clone(),
            epoch: None,
            source_epoch: None,
            failure: None,
            read_only: h.exports.contains(dest),
            reindex: h.reindexes.get(dest).cloned(),
            created_ms: h.copy_times.get(dest).map(|(c, _)| *c),
            ended_ms: None,
            as_of: h.copy_as_of.get(dest).copied(),
            // A stale run's entry is not decoded (`Head::encode` keeps only the record's run).
            progress: h.copy_progress.get(dest).copied(),
        });
    }
    let o = h.copies.get(dest)?;
    let (state, source_epoch, failure) = match o.end {
        CopyEnd::Succeeded { source_epoch } => (CopyState::Succeeded, Some(source_epoch), None),
        CopyEnd::Failed(f) => (CopyState::Failed, None, Some(f)),
        CopyEnd::Cancelled => (CopyState::Cancelled, None, None),
    };
    Some(CopyStatus {
        state,
        run: o.run,
        source: o.source.clone(),
        epoch: Some(o.epoch),
        source_epoch,
        failure,
        read_only: h.exports.contains(dest),
        reindex: h.reindexes.get(dest).cloned(),
        created_ms: h.copy_times.get(dest).map(|(c, _)| *c),
        // An ended copy's record is always stamped, never 0 (`stamp_end`).
        ended_ms: h.copy_times.get(dest).map(|(_, e)| *e),
        as_of: h.copy_as_of.get(dest).copied(),
        progress: None,
    })
}

/// Why a reindex's options cannot apply to the source schema `s` (M168): a further text
/// field or regex attribute named as its primary text field, or as an attribute it types
/// other than string, or more further fields than [`head::MAX_TEXTS`].
fn check_reindex(r: &Reindex, s: Option<&IndexSchema>) -> Result<(), String> {
    if r.texts.len() > head::MAX_TEXTS {
        return Err(format!(
            "a reindex declares at most {} further text fields",
            head::MAX_TEXTS
        ));
    }
    for name in r.texts.keys().chain(&r.trigram) {
        if name == crate::ID_KIND {
            return Err("\"id\" is not an attribute: it is neither text nor sketched".to_owned());
        }
    }
    let Some(s) = s else {
        return Ok(());
    };
    for name in r.texts.keys().chain(&r.trigram) {
        if !s.text_field.is_empty() && *name == s.text_field {
            return Err(format!(
                "{name:?} is the source's primary text field: declare its options under it"
            ));
        }
        if let Some(k) = s.types.get(name) {
            return Err(format!(
                "{name:?} is a {} attribute: a text field or regex attribute is a string",
                k.name()
            ));
        }
    }
    Ok(())
}

/// The copy of `source` into `dest` a repeated request started (M166): running, paused,
/// or succeeded while `dest` is still an index; and asking the same flag (M167).
fn same_copy(h: &Head, dest: &str, source: &ReplicaSource, ask: &Ask) -> Option<CopyStatus> {
    copy_status_in(h, dest).filter(|s| {
        &s.source == source
            && s.read_only == ask.read_only
            && s.reindex == ask.reindex
            && s.as_of == ask.as_of
            && match s.state {
                CopyState::Running | CopyState::Paused => true,
                CopyState::Succeeded => h.indexes.contains_key(dest),
                CopyState::Failed | CopyState::Cancelled => false,
            }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M196: `answerable` agrees with `Head::as_of` at every epoch, both edges included.
    #[test]
    fn answerable_is_what_as_of_answers() {
        let h = Head {
            epoch: Epoch(10),
            reaped_before: 5,
            ..Head::default()
        };
        for e in 0..13 {
            assert_eq!(answerable(&h, e), h.as_of(Epoch(e)).is_ok(), "epoch {e}");
        }
        assert!(answerable(&h, 5) && answerable(&h, 10));
        assert!(!answerable(&h, 4) && !answerable(&h, 11));
    }

    /// M166: a succeeded outcome answers a repeated request only while `dest` is an index --
    /// however the index stopped being one.
    #[test]
    fn a_success_is_matched_only_while_its_index_exists() {
        let source = ReplicaSource::local(TenantId(1), "src");
        let mut h = Head::default();
        h.record_copy(
            "dst",
            CopyOutcome {
                source: source.clone(),
                run: 3,
                epoch: 4,
                end: CopyEnd::Succeeded { source_epoch: 2 },
            },
        );
        assert_eq!(same_copy(&h, "dst", &source, &Ask::default()), None);
        h.indexes.insert("dst".to_owned(), Vec::new());
        let got = same_copy(&h, "dst", &source, &Ask::default()).map(|s| (s.state, s.run, s.epoch));
        assert_eq!(got, Some((CopyState::Succeeded, 3, Some(4))));
        let other = ReplicaSource::local(TenantId(1), "other");
        assert_eq!(same_copy(&h, "dst", &other, &Ask::default()), None);
    }

    #[test]
    fn a_named_fields_width_is_part_of_the_fingerprint() {
        // M98 review: a fold may record a width and change no segment (every row set aside),
        // and a replica that compared equal would keep the schema without it.
        let mut h = Head::default();
        h.indexes.insert(
            "i".to_owned(),
            vec![head::SegmentRef {
                key: "k/0.seg".to_owned(),
                rows: 1,
                dicts: None,
                len: None,
                ann: None,
            }],
        );
        h.schemas
            .insert("i".to_owned(), head::IndexSchema::default());
        let before = fingerprint(&h, "i");
        let mut w = h.clone();
        if let Some(s) = w.schemas.get_mut("i") {
            s.fields.insert("late".to_owned(), 3);
        }
        assert_ne!(fingerprint(&w, "i"), before);
        if let Some(s) = w.schemas.get_mut("i") {
            s.fields.insert("late".to_owned(), 4);
        }
        let four = fingerprint(&w, "i");
        if let Some(s) = w.schemas.get_mut("i") {
            s.fields.insert("late".to_owned(), 3);
            s.fields.insert("other".to_owned(), 4);
        }
        assert_ne!(fingerprint(&w, "i"), four, "names and widths both count");
    }

    #[test]
    fn a_suffixed_name_is_not_a_copy() {
        // M23: `_`, not `-`, so a segment or vector whose name was refused once never reads as
        // a replica's copy of a source hashing to its suffix.
        let base = "0046/tnt/70/idx/d/seg/L1/00000000000000000007-0000000000000001";
        let seg = format!("{base}.seg");
        let dv = format!("{seg}.{:020}-{:016x}.dv", 9, 1);
        for n in [1, 2, 0xa] {
            assert_eq!(carried(&crate::suffixed(&seg, n), ".seg"), None);
            assert_eq!(carried(&crate::suffixed(&dv, n), ".dv"), None);
        }
        // A copy is still one.
        assert_eq!(
            carried(&format!("{base}-{:016x}.seg", 0xab), ".seg"),
            Some(0xab)
        );
        assert_eq!(
            carried(
                &format!("{base}.seg.{:020}-{:016x}-{:016x}.dv", 9, 1, 0xcd),
                ".dv"
            ),
            Some(0xcd)
        );
    }
}
