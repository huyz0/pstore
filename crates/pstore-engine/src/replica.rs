//! Replication (M22): a replica index follows a source index -- in this tenant, another
//! tenant, or another store -- by pulling the source's folded state and committing it to its
//! own HEAD. See `docs/milestones/M22/SPEC.md`.

use crate::head::{self, Head, HeadAt, IndexSchema, ReplicaSource, Replication, fnv1a};
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

/// The sidecars a segment may have, each derived from its key and each optional -- by the
/// same functions that write them and that GC derives them with, so the three never drift.
fn sidecars(segment: &Key) -> [Key; 3] {
    [
        pstore_index::vec_index::centroid_key(segment),
        pstore_format::sparse::dict_key(segment),
        pstore_format::text::dict_key(segment),
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
    /// `(dest, run)` to the source fingerprint it last committed or found current.
    known: BTreeMap<(String, u64), u64>,
}

impl SyncState {
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
    /// `(dest segment, g)` to the dest vector key holding it.
    dvs: BTreeMap<(String, u64), String>,
    /// Every segment and vector key written, recorded before its PUT, so one that fails
    /// partway is buried too (M19). A segment's sidecars follow it to GC.
    written: Vec<String>,
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
type Copied = (u64, String, Result<bool, CopyErr>);

/// Why copying one source segment did not finish.
enum CopyErr {
    /// The source no longer has it: remap against a fresh source HEAD.
    Missing,
    /// Anything else: this replication fails this time.
    Failed(String),
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
            match head::commit(&*self.store, self.tenant, &at, &next).await {
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
        let there = head::read(from, source.tenant).await?;
        if fingerprint(&there.head, &source.index).is_none() {
            return refuse(ReplicaRefusal::SourceNotFound(source.index));
        }
        self.commit_change(|now, next| {
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
            Ok(Some(()))
        })
        .await
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
    /// ⚠️ **One at a time per engine.** Two concurrent calls derive the same copy keys (same
    /// lane, same epoch), and one burying a key the other is about to commit lets a GC reap it
    /// -- the lane-key hazard `bury_abandoned` documents for compaction. The worker runs one
    /// sync per tenant at a time; two processes have two lanes.
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
        for (d, r) in &plan {
            let read = heads
                .get(&(r.source.store.clone(), r.source.tenant))
                .cloned()
                .unwrap_or_else(|| Err("no read of this source".to_owned()));
            match &read {
                Err(e) => {
                    out.failed.insert(d.clone(), e.clone());
                }
                Ok(h) => match fingerprint(h, &r.source.index) {
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
        if changed.is_empty() {
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
                if out.failed.contains_key(d.as_str()) {
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
                    match self
                        .mirror(&src, &r.source, src_head, d, &at.head, e, &mut copies)
                        .await
                    {
                        Ok(m) => {
                            mapped.insert(d.clone(), m);
                        }
                        Err(why) => {
                            out.failed.insert(d.clone(), why);
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
            let mut used: BTreeSet<String> = BTreeSet::new();
            for (d, _) in &included {
                if let Some(m) = mapped.get(d.as_str()) {
                    self.apply(&mut next, d, m, &mut used);
                }
            }
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
            if included.is_empty() && unused.is_empty() {
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
            match head::commit(&*self.store, self.tenant, &at, &next).await {
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
        dest: &str,
        dest_head: &Head,
        e: u64,
        copies: &mut Copies,
    ) -> Result<Mapped, String> {
        let index = source.index.as_str();
        let mut stalls = 0;
        loop {
            let Some(fp) = fingerprint(src_head, index) else {
                return Err(format!("the source index {index:?} does not exist"));
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
            let results: Vec<Copied> =
                futures_util::stream::iter(todo.into_iter().map(|(from, h, to)| {
                    let written = &written;
                    async move {
                        let r = self.copy_segment(src, &from, &to, written).await;
                        (h, to, r)
                    }
                }))
                .buffer_unordered(IN_FLIGHT)
                .collect()
                .await;
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
                    Ok(lacked) => {
                        progress += 1;
                        copies.segs.insert((dest.to_owned(), h), to);
                        if lacked {
                            absent.push(h);
                        }
                    }
                    Err(CopyErr::Missing) => missing = true,
                    Err(CopyErr::Failed(why)) => return Err(why),
                }
            }
            // The delete vectors: copied when the source's key differs from the one the
            // dest's vector was copied from (its `g`), never by count (spec review). Every copy
            // in one round of GETs and PUTs (code review M2).
            let mut dvs = BTreeMap::new();
            let mut mapped = Vec::new();
            let mut fetch: Vec<(String, u64, String, u32, String)> = Vec::new();
            for r in &refs {
                let h = fnv1a(r.key.as_bytes());
                let Some(seg) = dest_of(h, copies) else {
                    continue;
                };
                mapped.push(SegmentRef {
                    key: seg.clone(),
                    rows: r.rows,
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
                    dvs.insert(seg, Some((d.clone(), n)));
                    continue;
                }
                let to = format!("{seg}.{e:020}-{:016x}-{g:016x}.dv", self.lane.0);
                fetch.push((seg, g, k, n, to));
            }
            copies
                .written
                .extend(fetch.iter().map(|(.., to)| to.clone()));
            let got: Vec<_> = futures_util::stream::iter(fetch.into_iter().map(
                |(seg, g, k, n, to)| async move {
                    let r = match src.get(&Key::new(k)).await {
                        Ok(bytes) => self
                            .store
                            .put(&Key::new(to.clone()), bytes)
                            .await
                            .map(|_| ())
                            .map_err(|e| CopyErr::Failed(e.to_string())),
                        Err(e) => Err(CopyErr::from(e)),
                    };
                    (seg, g, n, to, r)
                },
            ))
            .buffer_unordered(IN_FLIGHT)
            .collect()
            .await;
            for (seg, g, n, to, r) in got {
                match r {
                    Ok(()) => {
                        progress += 1;
                        copies.dvs.insert((seg.clone(), g), to.clone());
                        dvs.insert(seg, Some((to, n)));
                    }
                    Err(CopyErr::Missing) => missing = true,
                    Err(CopyErr::Failed(why)) => return Err(why),
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
                .map_err(|e| e.to_string())?
                .head;
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
                return Err(format!(
                    "the source index {index:?} kept changing: {MAX_STALLS} remaps copied nothing"
                ));
            }
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
    ) -> Result<bool, CopyErr> {
        let (from, to) = (Key::new(from), Key::new(to));
        let (seg, side) = futures_util::future::join(
            src.get(&from),
            futures_util::future::join_all(
                sidecars(&from).map(|k| async move { src.get(&k).await }),
            ),
        )
        .await;
        let seg = seg?;
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
        Ok(absent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
