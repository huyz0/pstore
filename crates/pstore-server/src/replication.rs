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
use pstore_engine::{
    CopyFailure, CopyState, CopyStatus, EngineError, ReplicaRefusal, ReplicaSource, Replication,
    Sources, SyncState,
};
use pstore_jobs::{Claim, Entry, JobsError, Register};
use pstore_types::TenantId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
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
    /// The bound on a replica's lag behind a source in this store (M192): its source is
    /// folded every half of it. `None`: no bound, and the source folds on its own triggers.
    pub max_lag: Option<Duration>,
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
            max_lag: None,
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
    /// M192: bounded replications it holds whose lag is past the bound, at its last tick.
    pub(crate) lagging: AtomicUsize,
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
            lagging: AtomicUsize::new(0),
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
    /// M192: the start of the last source fold this replication is known current through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fresh_ms: Option<u64>,
    /// M192: the bound the worker holds it to; `None` when it holds it to none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_lag_ms: Option<u64>,
    /// M192: its lag was past the bound at the worker's last look.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    lag_exceeded: bool,
}

/// The replications whose lag is past their bound, by these notes.
fn exceeded(notes: &Notes) -> BTreeSet<&String> {
    notes
        .iter()
        .filter(|(_, n)| n.lag_exceeded)
        .map(|(d, _)| d)
        .collect()
}

/// What a status must not wait `ttl / 3` to see (M192): which replications are past their
/// bound, and which have a fresh time at all.
fn urgent(notes: &Notes) -> (BTreeSet<&String>, BTreeSet<&String>) {
    let known = notes
        .iter()
        .filter(|(_, n)| n.fresh_ms.is_some())
        .map(|(d, _)| d)
        .collect();
    (exceeded(notes), known)
}

/// Whole milliseconds, saturating.
fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// What a source fold's failure is recorded as, which its next success clears.
const FOLD_ERROR: &str = "source fold: ";

/// Each bounded replication's fold: when it started, as an instant and as wall time, or why
/// it failed.
type Folded = BTreeMap<String, Result<(Instant, u64), String>>;

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

/// One slot of the worker's count, reserved by a control call: given back when dropped,
/// unless handed to the worker first.
struct Reservation<'a>(Option<&'a AtomicUsize>);

impl Reservation<'_> {
    /// The slot is the worker's now: dropping this gives nothing back.
    fn hand_over(&mut self) {
        self.0 = None;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if let Some(held) = self.0 {
            let _ = held.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |h| {
                Some(h.saturating_sub(1))
            });
        }
    }
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
    /// M192: when this worker first saw each bounded replication: its lag's base before any
    /// fold. Per replication, so one created on a held tenant does not start late.
    seen: BTreeMap<String, Instant>,
    /// M192: when its sources are folded next.
    fold_due: Instant,
    /// M192: each bounded replication's fresh time, as an instant and as wall time.
    fresh: BTreeMap<String, (Instant, u64)>,
}

impl Held {
    /// M192: after one sync, each bounded replication's fresh time, note and verdict. A
    /// replication is bounded when its source is in this store; `ok` are those the sync
    /// committed or found current. Answers how many went past the bound.
    fn judge(
        &mut self,
        sources: &BTreeMap<String, ReplicaSource>,
        folded: Option<&Folded>,
        ok: &BTreeSet<String>,
        lag: Duration,
        now: Instant,
    ) -> u64 {
        // Only what runs now: a cancelled name re-created must not inherit its past.
        self.seen.retain(|d, _| sources.contains_key(d));
        self.fresh.retain(|d, _| sources.contains_key(d));
        let mut into = 0;
        for d in sources
            .iter()
            .filter(|(_, s)| s.store.is_empty())
            .map(|(d, _)| d)
        {
            let n = self.notes.entry(d.clone()).or_default();
            match folded.and_then(|f| f.get(d)) {
                Some(Ok(at)) if ok.contains(d) => {
                    self.fresh.insert(d.clone(), *at);
                    if n.last_error
                        .as_deref()
                        .is_some_and(|e| e.starts_with(FOLD_ERROR))
                    {
                        n.last_error = None;
                    }
                }
                Some(Err(e)) => n.last_error = Some(clipped(&format!("{FOLD_ERROR}{e}"))),
                _ => {}
            }
            let first = *self.seen.entry(d.clone()).or_insert(now);
            let fresh = self.fresh.get(d);
            let base = fresh.map_or(first, |(i, _)| *i);
            let past = now.saturating_duration_since(base) > lag;
            if past && !n.lag_exceeded {
                into += 1;
            }
            n.lag_exceeded = past;
            n.fresh_ms = fresh.map(|(_, w)| *w);
            n.max_lag_ms = Some(ms(lag));
        }
        into
    }
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

    /// Folds `tenant` through this process's engine for it (M192), counted by outcome. Any
    /// process may fold any tenant: the commit is a CAS on its HEAD.
    async fn fold_source(&self, tenant: TenantId) -> Result<(), String> {
        let out = self.engine(tenant).await.fold_committed().await;
        let outcome = match &out {
            Ok(true) => "folded",
            Ok(false) => "nothing",
            Err(_) => "failed",
        };
        *self
            .http
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .source_folds
            .entry(outcome)
            .or_default() += 1;
        out.map(|_| ()).map_err(|e| e.to_string())
    }

    /// Folds each distinct source tenant in this store, together (M192), and answers each
    /// replication's outcome by its source tenant. A source in a remote store is not folded:
    /// its credentials are read-only, and [`Held::judge`] never reads a remote one's entry.
    async fn fold_sources(&self, sources: &BTreeMap<String, ReplicaSource>) -> Folded {
        let started = (Instant::now(), self.now_ms());
        let tenants: BTreeSet<TenantId> = sources
            .values()
            .filter(|s| s.store.is_empty())
            .map(|s| s.tenant)
            .collect();
        let outcomes: BTreeMap<TenantId, Result<(), String>> = futures_util::future::join_all(
            tenants
                .into_iter()
                .map(|t| async move { (t, self.fold_source(t).await) }),
        )
        .await
        .into_iter()
        .collect();
        sources
            .iter()
            .filter_map(|(d, s)| {
                let out = outcomes.get(&s.tenant)?.clone();
                Some((d.clone(), out.map(|()| started)))
            })
            .collect()
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
    pub(crate) fn source_store(&self, tenant: TenantId, store: &str) -> Option<Arc<dyn BlobStore>> {
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
        // ⚠️ **Held by a guard** (code review round 4): a client that goes away drops this
        // call wherever it waits, and a slot given back only by the code after the await
        // would be held by nobody, for good -- enough of them and the worker takes no work.
        let reserved = (self.replicating.worker.load(Ordering::SeqCst)
            && self
                .replicating
                .held
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |h| {
                    (h < policy.max).then_some(h + 1)
                })
                .is_ok())
        .then(|| Reservation(Some(&self.replicating.held)));
        let claim = reserved.is_some().then(|| Claim {
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
            // The reserved slot goes back with the failure, as the guard drops.
            Err(e) => return Err(e.into()),
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
            // Handed over: the slot is the worker's now.
            if let Some(mut r) = reserved {
                r.hand_over();
            }
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
            // Its slot was reserved when the claim was made: it takes it, or gives it back.
            if let std::collections::btree_map::Entry::Vacant(v) = w.held.entry(tenant) {
                v.insert(held(shard, generation, now, policy.period));
                w.renew_at.entry(shard).or_insert(now + renew_every);
                tick.claimed += 1;
            } else {
                self.release(1);
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
            // Against the count of what is held AND reserved by control calls in flight.
            let room = policy
                .max
                .saturating_sub(self.replicating.held.load(Ordering::SeqCst) + taken);
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
                let full = self.replicating.held.load(Ordering::SeqCst) >= policy.max;
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
                        self.replicating.held.fetch_add(1, Ordering::SeqCst);
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
        let mut ended = Vec::new();
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
            // M192: the sources folded first, every half of the bound, so the sync carries
            // everything they acknowledged before the fold began.
            let folded = match (policy.max_lag, h.st.sources()) {
                (Some(lag), Some(srcs)) if now >= h.fold_due => {
                    h.fold_due = now + lag / 2;
                    Some(self.fold_sources(&srcs).await)
                }
                _ => None,
            };
            let mut ok = BTreeSet::new();
            tick.synced += 1;
            let changed = match engine.replicate(&sources, &mut h.st).await {
                Ok(out) => {
                    ok.extend(out.committed.iter().cloned());
                    ok.extend(out.current.iter().cloned());
                    // M166: a copy that ended no longer runs: the register's `gen` says so.
                    if !out.finished.is_empty() {
                        ended.push(tenant);
                    }
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
            if let (Some(lag), Some(srcs)) = (policy.max_lag, h.st.sources()) {
                let into = h.judge(&srcs, folded.as_ref(), &ok, lag, Instant::now());
                self.http
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .lag_exceeded += into;
                if srcs.values().any(|s| s.store.is_empty()) {
                    h.due = h.due.min(h.fold_due);
                }
            }
            // Notes: only when they changed, and at most once per `ttl / 3` -- except a
            // replication going past its bound or back, or getting its first fresh time,
            // which is written at once (M192).
            let stale = h.written.as_ref().is_none_or(|(at, n)| {
                *n != h.notes && (now >= *at + renew_every || urgent(n) != urgent(&h.notes))
            });
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
        for tenant in ended {
            if !gone.contains(&tenant) {
                let _ = self.reconcile(tenant, false).await;
            }
        }
        for tenant in gone {
            if w.held.remove(&tenant).is_some() {
                self.release(1);
            }
            let _ = self.reconcile(tenant, false).await;
        }
        // ⚠️ Never stored over (code review round 2, m8; a race test found it): a control call
        // reserves its slot mid-tick, and a count rewritten from what the worker holds would
        // drop that reservation and let the next claim past `max`. Kept exact at each change.
        tick.held = w.held.len();
        self.replicating.lagging.store(
            w.held.values().map(|h| exceeded(&h.notes).len()).sum(),
            Ordering::SeqCst,
        );
        tick
    }

    /// Gives back `n` slots of the worker's count of what it holds and has reserved.
    fn release(&self, n: usize) {
        let _ = self
            .replicating
            .held
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |h| {
                Some(h.saturating_sub(n))
            });
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
            let before = w.held.len();
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
            self.release(before - w.held.len());
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
        // M192: what the worker bounds it to, and how far behind it was last known to be.
        out.insert("max_lag_ms".to_owned(), json!(note.max_lag_ms));
        out.insert("fresh_ms".to_owned(), json!(note.fresh_ms));
        out.insert(
            "lag_ms".to_owned(),
            json!(note.fresh_ms.map(|f| self.now_ms().saturating_sub(f))),
        );
        out.insert("lag_exceeded".to_owned(), json!(note.lag_exceeded));
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
        seen: BTreeMap::new(),
        fold_due: now,
        fresh: BTreeMap::new(),
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
    /// A copy's alone (M167): finish read-only, an export.
    read_only: Option<bool>,
    /// A copy's alone (M168): the text and regex options to rebuild under, a reindex.
    schema: Option<Value>,
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
    axum::Extension(principal): axum::Extension<crate::auth::Principal>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let tenant = tenant_of(&headers)?;
    let (source, from, read_only, schema) =
        api.parse_source(tenant, &principal, &body, "a replication's body")?;
    if read_only.is_some() {
        return Err(ApiError::bad_request(
            "read_only is a copy's: a replication follows its source and is always read-only",
        ));
    }
    if schema.is_some() {
        return Err(ApiError::bad_request(
            "schema is a copy's: a replication follows its source's schema",
        ));
    }
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

/// A create's source, the store to read it from, and a copy's `read_only` (M167) and
/// `schema` (M168).
type Parsed = (
    ReplicaSource,
    Arc<dyn BlobStore>,
    Option<bool>,
    Option<pstore_engine::Reindex>,
);

impl<S: BlobStore + 'static> Api<S> {
    /// A create's body (M22, and M166's copy): the source it names, authorized for this
    /// principal before anything is read, the store to read it from, and a copy's
    /// `read_only` (M167).
    fn parse_source(
        &self,
        tenant: TenantId,
        principal: &crate::auth::Principal,
        body: &[u8],
        what: &str,
    ) -> Result<Parsed, ApiError> {
        let req: CreateBody = serde_json::from_slice(body)
            .map_err(|e| ApiError::bad_request(format!("{what}: {e}")))?;
        // M168: checked before the source is authorized or read.
        let schema = req.schema.as_ref().map(crate::reindex_of).transpose()?;
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
        principal.source(source_tenant, &store)?;
        let Some(from) = self.source_store(tenant, &store) else {
            return Err(ApiError::bad_request(format!(
                "this server has no source store named {store:?}; configure it with PSTORE_SOURCES"
            )));
        };
        let source = ReplicaSource {
            store,
            tenant: source_tenant,
            index,
        };
        Ok((source, from, req.read_only, schema))
    }

    /// The copy into `dest`'s status (M166): HEAD and the worker's notes, in one round.
    async fn copy_status_of(&self, tenant: TenantId, dest: &str) -> Result<Value, ApiError> {
        let engine = self.engine(tenant).await;
        let store = self.store.as_tenant(tenant);
        let (status, notes) = futures_util::future::join(engine.copy_status(dest), async {
            match store.get(&notes_key(tenant)).await {
                Ok(b) => Ok(serde_json::from_slice::<Notes>(&b).unwrap_or_default()),
                Err(BlobError::NotFound(_)) => Ok(Notes::new()),
                Err(e) => Err(e),
            }
        })
        .await;
        let Some(status) = status? else {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "copy_not_found",
                format!("the index {dest:?} has no copy running and no recent outcome"),
            ));
        };
        let mut notes = notes.map_err(|e| EngineError::Blob(e.to_string()))?;
        let last_error = match status.state {
            CopyState::Running | CopyState::Paused => notes
                .remove(dest)
                .or_else(|| notes.remove(TENANT_NOTE))
                .and_then(|n| n.last_error),
            CopyState::Succeeded | CopyState::Failed | CopyState::Cancelled => None,
        };
        Ok(describe_copy(dest, &status, last_error))
    }
}

/// A copy's status as the wire says it (M166): `state` is the machine state.
fn describe_copy(dest: &str, s: &CopyStatus, last_error: Option<String>) -> Value {
    json!({
        "index": dest,
        "state": match s.state {
            CopyState::Running => "running",
            CopyState::Paused => "paused",
            CopyState::Succeeded => "succeeded",
            CopyState::Failed => "failed",
            CopyState::Cancelled => "cancelled",
        },
        "run": s.run,
        "source": {
            "store": s.source.store,
            "tenant": s.source.tenant.0.to_string(),
            "index": s.source.index,
        },
        "epoch": s.epoch,
        "source_epoch": s.source_epoch,
        "code": s.failure.map(|f| match f {
            CopyFailure::SourceNotFound => "source_not_found",
            CopyFailure::RebuildRefused => "rebuild_refused",
        }),
        "last_error": last_error,
        "read_only": s.read_only,
        // M194: wall-clock times, as the summary's (M148).
        "created_at": s.created_ms.map(crate::rfc3339_ms),
        "ended_at": s.ended_ms.map(crate::rfc3339_ms),
    })
}

/// `POST /v1/indexes/{dest}/copy` (M166): copies the source into the new `dest`, as a
/// replication that finishes in the commit that copies. `202` when this request started it;
/// `200` with the copy a repeated request started. With `read_only: true`, an export
/// (M167): it finishes read-only. With `schema`, a reindex (M168): every segment is rebuilt
/// under the text and regex options it declares.
pub(crate) async fn copy<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(dest): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<crate::auth::Principal>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let tenant = tenant_of(&headers)?;
    let (source, from, read_only, schema) =
        api.parse_source(tenant, &principal, &body, "a copy's body")?;
    let engine = api.engine(tenant).await;
    let read_only = read_only == Some(true);
    let (status, started) = match schema {
        // M168: a reindex rebuilds under the options; read-only at its finish when asked.
        Some(r) => {
            engine
                .create_reindex(&dest, source, &*from, read_only, r)
                .await?
        }
        None if read_only => engine.create_export(&dest, source, &*from).await?,
        None => engine.create_copy(&dest, source, &*from).await?,
    };
    if !started {
        return Ok((
            StatusCode::OK,
            axum::Json(describe_copy(&dest, &status, None)),
        )
            .into_response());
    }
    // As a create's: the change has happened, whatever the register says.
    let _ = api.reconcile(tenant, true).await;
    Ok((
        StatusCode::ACCEPTED,
        axum::Json(describe_copy(&dest, &status, None)),
    )
        .into_response())
}

/// `GET /v1/indexes/{dest}/copy` (M166).
pub(crate) async fn copy_status<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(dest): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<Value>, ApiError> {
    let tenant = tenant_of(&headers)?;
    Ok(axum::Json(api.copy_status_of(tenant, &dest).await?))
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

/// `POST /v1/indexes/{dest}/replication/sync` (M192): folds the source when it is in this
/// store, then syncs the tenant's replications at once. With the source folded and `dest`
/// current, every write the source acknowledged before the call is on the replica, and
/// `fresh_ms` says from when.
pub(crate) async fn sync_now<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(dest): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<crate::auth::Principal>,
) -> Result<axum::Json<Value>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let engine = api.engine(tenant).await;
    let Some((r, _, _)) = engine.replication_status(&dest).await? else {
        return Err(EngineError::Replica(ReplicaRefusal::NotFound(dest)).into());
    };
    if !r.running {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "replication_paused",
            format!("the replication into {dest:?} is paused: resume it to sync it"),
        ));
    }
    // As a create's: the source's authority, before the source is touched.
    principal.source(r.source.tenant, &r.source.store)?;
    let unavailable =
        |why: String| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable", why);
    let fresh = if r.source.store.is_empty() {
        let at = api.now_ms();
        api.fold_source(r.source.tenant)
            .await
            .map_err(|e| unavailable(format!("{FOLD_ERROR}{e}")))?;
        Some(at)
    } else {
        None
    };
    let sources = TenantSources {
        remotes: &api.replication().1,
        tenant,
    };
    let out = engine
        .replicate(&sources, &mut SyncState::default())
        .await?;
    if let Some(why) = out.failed.get(&dest) {
        return Err(unavailable(format!("sync: {why}")));
    }
    let mut v = if out.finished.contains(&dest) {
        // A copy (M166) this sync ended: its entry goes, and its outcome is the answer.
        let _ = api.reconcile(tenant, false).await;
        api.copy_status_of(tenant, &dest).await?
    } else {
        api.status_of(tenant, &dest, false).await?
    };
    if let Value::Object(m) = &mut v {
        m.insert("source_folded".to_owned(), json!(fresh.is_some()));
        let current = out.committed.contains(&dest) || out.current.contains(&dest);
        if let Some(at) = fresh.filter(|_| current) {
            m.insert("fresh_ms".to_owned(), json!(at));
            m.insert("lag_ms".to_owned(), json!(api.now_ms().saturating_sub(at)));
        }
    }
    Ok(axum::Json(v))
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
    use super::{BTreeMap, BTreeSet, Duration, Folded, Instant, NOTE_BYTES, clipped, held};
    use pstore_engine::ReplicaSource;
    use pstore_types::TenantId;

    /// M192: the verdict at the bound and just past it, counted once, and a fold that was not
    /// followed by a current sync moving nothing.
    #[test]
    #[expect(
        clippy::indexing_slicing,
        reason = "a missing note is the test failing"
    )]
    fn judge_marks_past_the_bound_once_and_only_from_a_current_fold() {
        let t0 = Instant::now();
        let lag = Duration::from_secs(10);
        let mut h = held(0, 1, t0, Duration::from_secs(1));
        let srcs = BTreeMap::from([
            ("d".to_owned(), ReplicaSource::local(TenantId(1), "s")),
            (
                "far".to_owned(),
                ReplicaSource {
                    store: "far".to_owned(),
                    tenant: TenantId(1),
                    index: "s".to_owned(),
                },
            ),
        ]);
        let ok = BTreeSet::from(["d".to_owned(), "far".to_owned()]);
        let folded: Folded = BTreeMap::from([
            ("d".to_owned(), Ok((t0, 1_000))),
            ("far".to_owned(), Ok((t0, 1_000))),
        ]);
        assert_eq!(h.judge(&srcs, Some(&folded), &ok, lag, t0), 0);
        assert_eq!(h.notes["d"].fresh_ms, Some(1_000));
        // A remote source is never bounded, whatever a fold of its tenant id said.
        assert!(!h.notes.contains_key("far"));
        // Exactly at the bound: not past it.
        assert_eq!(h.judge(&srcs, None, &ok, lag, t0 + lag), 0);
        assert!(!h.notes["d"].lag_exceeded);
        // Past it: counted once, however many syncs see it.
        let late = t0 + lag + Duration::from_millis(1);
        assert_eq!(h.judge(&srcs, None, &ok, lag, late), 1);
        assert_eq!(h.judge(&srcs, None, &ok, lag, late), 0);
        assert!(h.notes["d"].lag_exceeded);
        // A fold whose sync did not find `d` current moves nothing.
        let refold: Folded = BTreeMap::from([("d".to_owned(), Ok((late, 2_000)))]);
        h.judge(&srcs, Some(&refold), &BTreeSet::new(), lag, late);
        assert_eq!(h.notes["d"].fresh_ms, Some(1_000));
        assert!(h.notes["d"].lag_exceeded);
        h.judge(&srcs, Some(&refold), &ok, lag, late);
        assert_eq!(h.notes["d"].fresh_ms, Some(2_000));
        assert!(!h.notes["d"].lag_exceeded);
    }

    #[test]
    fn a_note_is_clipped_at_a_char_boundary_below_the_limit() {
        // A two-byte char straddling the limit: cut before it, never through or past it.
        let s = format!("{}é tail", "a".repeat(NOTE_BYTES - 1));
        assert_eq!(clipped(&s), "a".repeat(NOTE_BYTES - 1));
        assert_eq!(clipped("short"), "short");
    }
}
