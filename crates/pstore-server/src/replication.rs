//! Replication jobs (M22.3): the HTTP surface, the per-process worker, and the named remote
//! sources. The engine does the replicating ([`Engine::replicate`]); this decides which
//! process does it for which tenant, through a register of tenants with running replications
//! kept in the blob store ([`pstore_jobs`]). See `docs/milestones/M22/SPEC.md`.
//!
//! ⚠️ **The register is advisory** (Design rule 12). Every replica commit is a CAS on a HEAD
//! that states the replication running, so two processes syncing one tenant waste copies and
//! never corrupt; a claim only keeps that rare.

use crate::{Api, ApiError, tenant_of};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use pstore_blob::{Accounted, BlobError, BlobStore, Key, TenantView};
use pstore_engine::{EngineError, ReplicaSource, Replication, Sources, SyncState};
use pstore_jobs::{Claim, Entry, JobsError, Register};
use pstore_types::TenantId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::time::Instant;

/// The register's name in the store.
const REGISTER: &str = "rep";

/// The tenant a worker's own register traffic is billed to: no tenant asked for it.
const SYSTEM: TenantId = TenantId(0);

/// How the worker runs (spec § Worker).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicationPolicy {
    /// How often a worker reads one more shard of the register, looking for unclaimed work.
    pub scan: Duration,
    /// How long a claim lasts unrenewed. Renewed every `ttl / 3`.
    pub ttl: Duration,
    /// Tenants one worker holds at most.
    pub max: usize,
    /// A tenant's sync interval while its sources change.
    pub period: Duration,
    /// The interval it doubles to while nothing changes.
    pub idle: Duration,
    /// The register's shard count, honoured only when the register is first created.
    pub shards: u16,
}

impl Default for ReplicationPolicy {
    fn default() -> Self {
        Self {
            scan: Duration::from_secs(10),
            ttl: Duration::from_secs(120),
            max: 64,
            period: Duration::from_secs(1),
            idle: Duration::from_secs(60),
            shards: 64,
        }
    }
}

/// The worker's policy and the named remote sources, each accounted.
type Configured = (
    ReplicationPolicy,
    BTreeMap<String, Accounted<Arc<dyn BlobStore>>>,
);

/// What an [`Api`] keeps for replication.
pub(crate) struct Replicating<S> {
    configured: OnceLock<Configured>,
    register: tokio::sync::OnceCell<Register<TenantView<S>>>,
    /// Whether this process's worker runs, so a create may claim its job for it.
    worker: AtomicBool,
    /// How many tenants it holds.
    held: AtomicUsize,
    /// Tenants a control call claimed for this process's worker, which it adopts next tick.
    adopted: Mutex<Vec<(TenantId, u16, u64)>>,
    /// The clock claims are written in: wall time at start, advanced by tokio's clock, so a
    /// paused test clock moves it.
    start: (u64, Instant),
}

impl<S> Default for Replicating<S> {
    fn default() -> Self {
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        Self {
            configured: OnceLock::new(),
            register: tokio::sync::OnceCell::new(),
            worker: AtomicBool::new(false),
            held: AtomicUsize::new(0),
            adopted: Mutex::new(Vec::new()),
            start: (wall, Instant::now()),
        }
    }
}

impl<S> std::fmt::Debug for Replicating<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replicating")
            .field("worker", &self.worker)
            .field("held", &self.held)
            .finish_non_exhaustive()
    }
}

/// One status note per replication, in `REPLSTATUS`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Note {
    last_commit_ms: Option<u64>,
    last_error: Option<String>,
}

type Notes = BTreeMap<String, Note>;

/// The status notes' key: beside the tenant's HEAD, derived from its id alone.
fn notes_key(tenant: TenantId) -> Key {
    Key::new(format!(
        "{:04x}/tnt/{}/REPLSTATUS",
        tenant.0 as u16, tenant.0
    ))
}

/// A tenant's id in the register.
fn id_of(tenant: TenantId) -> String {
    format!("{:032x}", tenant.0)
}

fn tenant_of_id(id: &str) -> Option<TenantId> {
    u128::from_str_radix(id, 16).ok().map(TenantId)
}

/// The note a tenant-wide sync failure is recorded under, which a status falls back to.
const TENANT_NOTE: &str = "*";

/// What `last_error` may hold.
const NOTE_BYTES: usize = 256;

fn clipped(s: &str) -> String {
    let mut end = s.len().min(NOTE_BYTES);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default().to_owned()
}

impl From<JobsError> for ApiError {
    fn from(e: JobsError) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            format!("replication register: {e}"),
        )
    }
}

/// The sources one tenant's sync may read, each billed to that tenant.
struct TenantSources<'a> {
    remotes: &'a BTreeMap<String, Accounted<Arc<dyn BlobStore>>>,
    tenant: TenantId,
}

impl Sources for TenantSources<'_> {
    fn store(&self, name: &str) -> Option<Arc<dyn BlobStore>> {
        self.remotes
            .get(name)
            .map(|a| Arc::new(a.as_tenant(self.tenant)) as Arc<dyn BlobStore>)
    }
}

/// What one tenant's holder keeps.
#[derive(Debug)]
struct Held {
    shard: u16,
    generation: u64,
    st: SyncState,
    due: Instant,
    interval: Duration,
    notes: Notes,
    written: Option<(Instant, Notes)>,
}

/// One process's replication worker: what it holds and when it next looks.
#[derive(Debug, Default)]
pub struct Worker {
    held: BTreeMap<TenantId, Held>,
    renew_at: BTreeMap<u16, Instant>,
    next_scan: Option<Instant>,
    cursor: u64,
}

/// What one [`Api::replication_tick`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerTick {
    /// Tenants held after the tick.
    pub held: usize,
    /// Tenants newly claimed or adopted.
    pub claimed: usize,
    /// Syncs run.
    pub synced: usize,
    /// Replications whose sync committed.
    pub committed: usize,
}

impl<S: BlobStore + 'static> Api<S> {
    /// Sets the worker's policy and the named remote sources (M22). Once: a second call is
    /// ignored and answers `false`. Unconfigured, the policy is the default and there are no
    /// remote sources.
    pub fn configure_replication(
        &self,
        policy: ReplicationPolicy,
        sources: BTreeMap<String, Arc<dyn BlobStore>>,
    ) -> bool {
        let sources = sources
            .into_iter()
            .map(|(n, s)| (n, Accounted::new(s)))
            .collect();
        self.replicating.configured.set((policy, sources)).is_ok()
    }

    fn replication(&self) -> &Configured {
        self.replicating
            .configured
            .get_or_init(|| (ReplicationPolicy::default(), BTreeMap::new()))
    }

    fn policy(&self) -> ReplicationPolicy {
        self.replication().0
    }

    fn now_ms(&self) -> u64 {
        let (wall, at) = self.replicating.start;
        wall.saturating_add(u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    /// The register, opened on first use: one GET of its `CONFIG` (and one CAS, the first
    /// time any process opens it), billed to no tenant.
    async fn register(&self) -> Result<&Register<TenantView<S>>, ApiError> {
        let shards = self.policy().shards;
        self.replicating
            .register
            .get_or_try_init(|| {
                Register::open(Arc::new(self.store.as_tenant(SYSTEM)), REGISTER, shards)
            })
            .await
            .map_err(ApiError::from)
    }

    /// The register as `tenant` uses it: its control calls are billed to it.
    async fn register_for(&self, tenant: TenantId) -> Result<Register<TenantView<S>>, ApiError> {
        Ok(self
            .register()
            .await?
            .on(Arc::new(self.store.as_tenant(tenant))))
    }

    /// Where `source` is read from, for `tenant`, billed to `tenant`.
    fn source_store(&self, tenant: TenantId, store: &str) -> Option<Arc<dyn BlobStore>> {
        if store.is_empty() {
            return Some(Arc::new(self.store.as_tenant(tenant)));
        }
        TenantSources {
            remotes: &self.replication().1,
            tenant,
        }
        .store(store)
    }

    /// Makes `tenant`'s register entry match its HEAD (spec § Queue): `touch` after a control
    /// call's HEAD CAS, so a racing reconcile holding the HEAD from before it loses. A new
    /// entry is claimed for this process's worker when it runs and has room, and handed to it.
    async fn reconcile(&self, tenant: TenantId, touch: bool) -> Result<Option<Entry>, ApiError> {
        let reg = self.register_for(tenant).await?;
        let engine = self.engine(tenant).await;
        let policy = self.policy();
        // ⚠️ A slot is reserved before the claim is offered, so two control calls racing a
        // tick cannot both claim past `max` on a stale count (code review M1).
        let reserved = self.replicating.worker.load(Ordering::SeqCst)
            && self
                .replicating
                .held
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |h| {
                    (h < policy.max).then_some(h + 1)
                })
                .is_ok();
        let claim = reserved.then(|| Claim {
            owner: self.lane.0,
            expires_ms: self
                .now_ms()
                .saturating_add(u64::try_from(policy.ttl.as_millis()).unwrap_or(u64::MAX)),
        });
        let entry = match reg
            .reconcile(
                &id_of(tenant),
                || {
                    let engine = Arc::clone(&engine);
                    async move {
                        engine
                            .replication_gen()
                            .await
                            .map_err(|e| JobsError::Want(e.to_string()))
                    }
                },
                claim,
                touch,
            )
            .await
        {
            Ok(e) => e,
            Err(e) => {
                // The reserved slot goes back with the failure (code review round 2, m8).
                if reserved {
                    self.replicating.held.fetch_sub(1, Ordering::SeqCst);
                }
                return Err(e.into());
            }
        };
        // Handed to the worker only when THIS call's claim is the one written; otherwise the
        // reserved slot is given back.
        // An existing claim by this lane counts too: the worker's own cleanup reconciling a
        // tenant that runs again keeps the claim it had, and must get the tenant back (m7).
        let ours = claim.is_some()
            && entry.is_some_and(|e| e.claim.is_some_and(|c| c.owner == self.lane.0));
        if ours && let Some(e) = entry {
            self.replicating
                .adopted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((tenant, reg.shard_of(&id_of(tenant)), e.generation));
        } else if reserved {
            self.replicating.held.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(entry)
    }

    /// Removes `tenant`'s entry whatever its HEAD says: what a crash between a control call's
    /// HEAD CAS and its reconcile leaves. Not part of the API.
    #[doc(hidden)]
    pub async fn forget_replication_entry_for_test(&self, tenant: TenantId) {
        if let Ok(reg) = self.register_for(tenant).await {
            let _ = reg
                .reconcile(&id_of(tenant), || async { Ok(None) }, None, true)
                .await;
        }
    }

    /// `tenant`'s claim as `(owner, expires_ms)`, read straight from its shard. Not part of the
    /// API.
    #[doc(hidden)]
    pub async fn replication_claim_for_test(&self, tenant: TenantId) -> Option<(u64, u64)> {
        let reg = self.register_for(tenant).await.ok()?;
        let id = id_of(tenant);
        reg.read(reg.shard_of(&id))
            .await
            .ok()?
            .entries
            .get(&id)?
            .claim
            .map(|c| (c.owner, c.expires_ms))
    }

    /// The worker's shared state, for its debug output. Not part of the API.
    #[doc(hidden)]
    pub fn replicating_for_test(&self) -> &impl std::fmt::Debug {
        &self.replicating
    }

    /// Runs only the worker's renewal of its due shards. Not part of the API.
    #[doc(hidden)]
    pub async fn renew_for_test(&self, w: &mut Worker) {
        if let Ok(reg) = self.register().await {
            self.renew_due(w, reg).await;
        }
    }

    /// Reads of the source store `name` billed to `tenant`. Not part of the API.
    #[doc(hidden)]
    pub fn source_reads_for_test(&self, name: &str, tenant: TenantId) -> u64 {
        self.replication()
            .1
            .get(name)
            .map_or(0, |a| a.count(tenant, pstore_blob::OpClass::Read))
    }

    /// `tenant`'s register entry as `(gen, claim owner)`, read straight from its shard.
    /// Not part of the API.
    #[doc(hidden)]
    pub async fn replication_entry_for_test(&self, tenant: TenantId) -> Option<(u64, Option<u64>)> {
        let reg = self.register_for(tenant).await.ok()?;
        let id = id_of(tenant);
        reg.read(reg.shard_of(&id))
            .await
            .ok()?
            .entries
            .get(&id)
            .map(|e| (e.generation, e.claim.map(|c| c.owner)))
    }

    /// Adds an entry for `tenant` whatever its HEAD says: what a crash between a cancel's
    /// HEAD CAS and its reconcile leaves. Not part of the API.
    #[doc(hidden)]
    pub async fn plant_replication_entry_for_test(&self, tenant: TenantId) {
        if let Ok(reg) = self.register_for(tenant).await {
            let _ = reg
                .reconcile(&id_of(tenant), || async { Ok(Some(1)) }, None, true)
                .await;
        }
    }

    /// One pass of this process's replication worker (spec § Worker): adopt what a control
    /// call claimed for it, scan one shard on schedule, renew what it holds on schedule, and
    /// sync each held tenant that is due, one at a time.
    pub async fn replication_tick(&self, w: &mut Worker) -> WorkerTick {
        self.replicating.worker.store(true, Ordering::SeqCst);
        let mut tick = WorkerTick::default();
        let policy = self.policy();
        let Ok(reg) = self.register().await else {
            return tick;
        };
        let now = Instant::now();
        let ttl_ms = u64::try_from(policy.ttl.as_millis()).unwrap_or(u64::MAX);
        let renew_every = policy.ttl / 3;

        // Handed over by this process's control calls.
        let adopted: Vec<_> = std::mem::take(
            &mut *self
                .replicating
                .adopted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (tenant, shard, generation) in adopted {
            // Its slot was reserved when the claim was made.
            if let std::collections::btree_map::Entry::Vacant(v) = w.held.entry(tenant) {
                v.insert(held(shard, generation, now, policy.period));
                w.renew_at.entry(shard).or_insert(now + renew_every);
                tick.claimed += 1;
            }
        }

        // Scan: at start every shard at once, so a restarted lane takes back its own; then
        // one shard per `scan`, rotating from an offset the lane picks.
        let scan: Vec<u16> = match w.next_scan {
            None => (0..reg.shards()).collect(),
            Some(at) if now >= at => {
                let s = (self.lane.0.wrapping_add(w.cursor)) % u64::from(reg.shards());
                w.cursor = w.cursor.wrapping_add(1);
                vec![u16::try_from(s).unwrap_or(0)]
            }
            Some(_) => Vec::new(),
        };
        if !scan.is_empty() {
            w.next_scan = Some(now + policy.scan);
        }
        // ⚠️ **One shard at a time, the room shrinking as it fills** (code review B1): the start
        // scan reads every shard, and the same room handed to each would take `max` from every
        // one of them. Sequential, so S round trips -- once, at start.
        let mut claimed = Vec::with_capacity(scan.len());
        let mut taken = 0;
        for s in &scan {
            let room = policy.max.saturating_sub(w.held.len() + taken);
            let got = reg
                .claim(*s, self.lane.0, self.now_ms(), ttl_ms, room)
                .await;
            if let Ok(g) = &got {
                taken += g
                    .keys()
                    .filter(|id| tenant_of_id(id).is_some_and(|t| !w.held.contains_key(&t)))
                    .count();
            }
            claimed.push(got);
        }
        for (shard, got) in scan.iter().zip(claimed) {
            let Ok(got) = got else { continue };
            for (id, e) in got {
                let Some(tenant) = tenant_of_id(&id) else {
                    continue;
                };
                let full = w.held.len() >= policy.max;
                match w.held.get_mut(&tenant) {
                    Some(h) if h.generation != e.generation => {
                        h.generation = e.generation;
                        h.st.invalidate();
                        h.due = now;
                    }
                    Some(_) => {}
                    // Past `max`, it is not held: the next renewal of this shard releases it.
                    None if full => continue,
                    None => {
                        w.held
                            .insert(tenant, held(*shard, e.generation, now, policy.period));
                        tick.claimed += 1;
                    }
                }
                w.renew_at.entry(*shard).or_insert(now + renew_every);
            }
        }

        self.renew_due(w, reg).await;

        // Sync each due tenant, one at a time: the engine's copy bound is then the worker's.
        let due: Vec<TenantId> = w
            .held
            .iter()
            .filter(|(_, h)| now >= h.due)
            .map(|(t, _)| *t)
            .collect();
        let mut gone = Vec::new();
        for tenant in due {
            // Before every sync, not once a tick: a long copy must not let every other claim
            // lapse behind it (code review M2).
            self.renew_due(w, reg).await;
            let Some(h) = w.held.get_mut(&tenant) else {
                continue;
            };
            let engine = self.engine(tenant).await;
            let sources = TenantSources {
                remotes: &self.replication().1,
                tenant,
            };
            tick.synced += 1;
            let changed = match engine.replicate(&sources, &mut h.st).await {
                Ok(out) => {
                    let ran = !(out.idle
                        && out.committed.is_empty()
                        && out.current.is_empty()
                        && out.failed.is_empty());
                    if !ran {
                        // Nothing runs here any more: the entry goes, and so does the hold.
                        gone.push(tenant);
                        continue;
                    }
                    tick.committed += out.committed.len();
                    let at = self.now_ms();
                    for d in &out.committed {
                        let n = h.notes.entry(d.clone()).or_default();
                        n.last_commit_ms = Some(at);
                        n.last_error = None;
                    }
                    for (d, why) in &out.failed {
                        h.notes.entry(d.clone()).or_default().last_error = Some(clipped(why));
                    }
                    // Only what runs now: a cancelled name re-created must not show its past.
                    h.notes.retain(|d, _| {
                        out.committed.contains(d)
                            || out.current.contains(d)
                            || out.failed.contains_key(d)
                    });
                    !out.committed.is_empty()
                }
                Err(e) => {
                    // The whole tenant's sync failed: recorded for every replication, under
                    // `*` when it has none yet, which a status falls back to.
                    let why = clipped(&e.to_string());
                    for n in h.notes.values_mut() {
                        n.last_error = Some(why.clone());
                    }
                    h.notes
                        .entry(TENANT_NOTE.to_owned())
                        .or_default()
                        .last_error = Some(why);
                    false
                }
            };
            h.interval = if changed {
                policy.period
            } else {
                (h.interval * 2).min(policy.idle)
            };
            h.due = now + h.interval;
            // Notes: only when they changed, and at most once per `ttl / 3`.
            let stale = h
                .written
                .as_ref()
                .is_none_or(|(at, n)| *n != h.notes && now >= *at + renew_every);
            if stale
                && !h.notes.is_empty()
                && let Ok(body) = serde_json::to_vec(&h.notes)
                && self
                    .store
                    .as_tenant(tenant)
                    .put(&notes_key(tenant), Bytes::from(body))
                    .await
                    .is_ok()
            {
                h.written = Some((now, h.notes.clone()));
            }
        }
        for tenant in gone {
            w.held.remove(&tenant);
            let _ = self.reconcile(tenant, false).await;
        }
        tick.held = w.held.len();
        let pending = self
            .replicating
            .adopted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        self.replicating
            .held
            .store(tick.held + pending, Ordering::SeqCst);
        tick
    }

    /// Renews each held shard whose time has come: one CAS per shard per `ttl / 3`, keeping
    /// exactly the tenants held there and releasing the rest. A tenant it lost, or whose
    /// entry is gone, is dropped; a changed `gen` re-reads HEAD at the next sync.
    async fn renew_due(&self, w: &mut Worker, reg: &Register<TenantView<S>>) {
        let policy = self.policy();
        let now = Instant::now();
        let ttl_ms = u64::try_from(policy.ttl.as_millis()).unwrap_or(u64::MAX);
        let renew_every = policy.ttl / 3;
        let due: Vec<u16> = w
            .renew_at
            .iter()
            .filter(|(_, at)| now >= **at)
            .map(|(s, _)| *s)
            .collect();
        for shard in due {
            // Held here, and handed over but not yet adopted: a control call can claim for
            // this worker mid-tick, and a renewal must not release that claim before the next
            // tick adopts it.
            let pending: Vec<TenantId> = self
                .replicating
                .adopted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|(_, s, _)| *s == shard)
                .map(|(t, _, _)| *t)
                .collect();
            let keep: std::collections::BTreeSet<String> = w
                .held
                .iter()
                .filter(|(_, h)| h.shard == shard)
                .map(|(t, _)| *t)
                .chain(pending)
                .map(id_of)
                .collect();
            let Ok(kept) = reg
                .renew(shard, self.lane.0, self.now_ms(), ttl_ms, &keep)
                .await
            else {
                continue;
            };
            w.renew_at.insert(shard, now + renew_every);
            w.held.retain(|t, h| {
                if h.shard != shard {
                    return true;
                }
                match kept.get(&id_of(*t)) {
                    None => false,
                    Some(e) => {
                        if e.generation != h.generation {
                            h.generation = e.generation;
                            h.st.invalidate();
                            h.due = now;
                        }
                        true
                    }
                }
            });
        }
        let shards: std::collections::BTreeSet<u16> = w.held.values().map(|h| h.shard).collect();
        w.renew_at.retain(|s, _| shards.contains(s));
    }

    /// A replication's status (spec § API): its HEAD record and index stats, its register
    /// entry, and its notes, read together. With `repair` (a `GET`), an entry that disagrees
    /// with HEAD is reconciled. ⚠️ A control call answers without it: its own reconcile is the
    /// touch the register's argument needs, and a repair here would hide one left out.
    async fn status_of(
        &self,
        tenant: TenantId,
        dest: &str,
        repair: bool,
    ) -> Result<Value, ApiError> {
        let engine = self.engine(tenant).await;
        let reg = self.register_for(tenant).await?;
        let id = id_of(tenant);
        let store = self.store.as_tenant(tenant);
        let (head, shard, notes) = futures_util::future::join3(
            engine.replication_status(dest),
            reg.read(reg.shard_of(&id)),
            async {
                match store.get(&notes_key(tenant)).await {
                    Ok(b) => Ok(serde_json::from_slice::<Notes>(&b).unwrap_or_default()),
                    Err(BlobError::NotFound(_)) => Ok(Notes::new()),
                    Err(e) => Err(e),
                }
            },
        )
        .await;
        let Some((r, stats, generation)) = head? else {
            return Err(
                EngineError::Replica(pstore_engine::ReplicaRefusal::NotFound(dest.to_owned()))
                    .into(),
            );
        };
        let mut entry = shard?.entries.get(&id).copied();
        if repair && entry.map(|e| e.generation) != generation {
            entry = self.reconcile(tenant, false).await?;
        }
        let mut notes = notes.map_err(|e| EngineError::Blob(e.to_string()))?;
        let note = notes
            .remove(dest)
            .or_else(|| notes.remove(TENANT_NOTE))
            .unwrap_or_default();
        let mut out = describe(dest, &r);
        out.insert(
            "segments".to_owned(),
            json!(stats.as_ref().map_or(0, |s| s.segments)),
        );
        out.insert(
            "rows".to_owned(),
            json!(stats.as_ref().map_or(0, |s| s.documents)),
        );
        out.insert("queued".to_owned(), json!(entry.is_some()));
        out.insert(
            "claim".to_owned(),
            match entry.and_then(|e| e.claim) {
                Some(c) => json!({"owner": c.owner, "expires_ms": c.expires_ms}),
                None => Value::Null,
            },
        );
        out.insert("last_commit_ms".to_owned(), json!(note.last_commit_ms));
        out.insert("last_error".to_owned(), json!(note.last_error));
        Ok(Value::Object(out))
    }
}

fn held(shard: u16, generation: u64, now: Instant, period: Duration) -> Held {
    Held {
        shard,
        generation,
        st: SyncState::default(),
        due: now,
        interval: period,
        notes: Notes::new(),
        written: None,
    }
}

/// The fields every answer about a replication carries.
fn describe(dest: &str, r: &Replication) -> serde_json::Map<String, Value> {
    let v = json!({
        "index": dest,
        "state": if r.running { "running" } else { "paused" },
        "source": {
            "store": r.source.store,
            "tenant": r.source.tenant.0.to_string(),
            "index": r.source.index,
        },
        "run": r.run,
        "applied_epoch": r.applied.map(|(e, _)| e),
        "rejected": r.rejected,
    });
    match v {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    source: SourceBody,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceBody {
    index: Option<String>,
    tenant: Option<String>,
    store: Option<String>,
}

/// `PUT /v1/indexes/{dest}/replication`: creates a running replication into the new `dest`.
pub(crate) async fn create<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(dest): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let tenant = tenant_of(&headers)?;
    let req: CreateBody = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("a replication's body: {e}")))?;
    let Some(index) = req.source.index else {
        return Err(ApiError::bad_request("source.index is required"));
    };
    let source_tenant = match req.source.tenant {
        None => tenant,
        Some(t) => TenantId(t.parse::<u128>().map_err(|_| {
            ApiError::bad_request(format!("source.tenant {t:?} is not a tenant id"))
        })?),
    };
    let store = req.source.store.unwrap_or_default();
    let Some(from) = api.source_store(tenant, &store) else {
        return Err(ApiError::bad_request(format!(
            "this server has no source store named {store:?}; configure it with PSTORE_SOURCES"
        )));
    };
    let source = ReplicaSource {
        store,
        tenant: source_tenant,
        index,
    };
    api.engine(tenant)
        .await
        .create_replication(&dest, source, &*from)
        .await?;
    // ⚠️ After the HEAD CAS the change has happened: a register that cannot be reached leaves
    // the entry stale -- `queued` says so, and a `GET` repairs it -- never a 503 for a create
    // that took (code review m3).
    let _ = api.reconcile(tenant, true).await;
    let out = api.status_of(tenant, &dest, false).await?;
    Ok((StatusCode::CREATED, axum::Json(out)).into_response())
}

/// `GET /v1/indexes/{dest}/replication`.
pub(crate) async fn status<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(dest): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<Value>, ApiError> {
    let tenant = tenant_of(&headers)?;
    Ok(axum::Json(api.status_of(tenant, &dest, true).await?))
}

/// `POST /v1/indexes/{dest}/replication/pause`. Idempotent.
pub(crate) async fn pause<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(dest): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<Value>, ApiError> {
    let tenant = tenant_of(&headers)?;
    api.engine(tenant).await.pause_replication(&dest).await?;
    let _ = api.reconcile(tenant, true).await;
    Ok(axum::Json(api.status_of(tenant, &dest, false).await?))
}

/// `POST /v1/indexes/{dest}/replication/resume`. Idempotent; also restores a lost entry.
pub(crate) async fn resume<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(dest): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<Value>, ApiError> {
    let tenant = tenant_of(&headers)?;
    api.engine(tenant).await.resume_replication(&dest).await?;
    let _ = api.reconcile(tenant, true).await;
    Ok(axum::Json(api.status_of(tenant, &dest, false).await?))
}

/// `DELETE /v1/indexes/{dest}/replication`: `dest` stays, as a normal index.
pub(crate) async fn cancel<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(dest): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<Value>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let engine = api.engine(tenant).await;
    let epoch = engine.cancel_replication(&dest).await?;
    let _ = api.reconcile(tenant, true).await;
    // The notes go with the last replication. A worker that committed just before may write
    // them back once: one orphan per tenant, reused if the tenant replicates again.
    if engine.replications().await?.is_empty() {
        let store = api.store.as_tenant(tenant);
        let _ = store.delete_batch(&[notes_key(tenant)]).await;
    }
    Ok(axum::Json(json!({"index": dest, "epoch": epoch.0})))
}

/// `GET /v1/replications`: the tenant's replications, after one HEAD read.
pub(crate) async fn list<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    headers: HeaderMap,
) -> Result<axum::Json<Value>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let all = api.engine(tenant).await.replications().await?;
    let out: Vec<Value> = all
        .iter()
        .map(|(d, r)| Value::Object(describe(d, r)))
        .collect();
    Ok(axum::Json(json!({ "replications": out })))
}

/// Runs [`Api::replication_tick`] every `policy.period` until `stop` resolves, as
/// [`crate::run_folds`] runs folds: ticks never overlap, and a stop mid-tick lets the tick
/// finish, whose commits are each a CAS or nothing.
pub async fn run_replication<S: BlobStore + 'static>(
    api: Arc<Api<S>>,
    stop: impl std::future::Future<Output = ()> + Send,
) {
    let mut w = Worker::default();
    let period = api.policy().period;
    tokio::pin!(stop);
    loop {
        let tick = api.replication_tick(&mut w);
        tokio::pin!(tick);
        tokio::select! {
            () = &mut stop => {
                tick.await;
                break;
            }
            _ = &mut tick => {}
        }
        tokio::select! {
            () = &mut stop => break,
            () = tokio::time::sleep(period) => {}
        }
    }
    api.replicating.worker.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::{NOTE_BYTES, clipped};

    #[test]
    fn a_note_is_clipped_at_a_char_boundary_below_the_limit() {
        // A two-byte char straddling the limit: cut before it, never through or past it.
        let s = format!("{}é tail", "a".repeat(NOTE_BYTES - 1));
        assert_eq!(clipped(&s), "a".repeat(NOTE_BYTES - 1));
        assert_eq!(clipped("short"), "short");
    }
}
