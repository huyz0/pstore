//! The HTTP surface — **the first door onto the engine** (M7c).
//!
//! `11-design/api-design.md` states the four principles this implements: the index is the
//! noun, every tradeoff is a client parameter, every response is honest about what it cost
//! and how fresh it is, and batches are the natural unit.
//!
//! M127 binds tenants and operation scopes to locally verified bearer credentials.
//! Quotas, TLS termination and deployment isolation remain operator work.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use pstore_blob::{Accounted, BlobStore, OpClass, TenantView};
use pstore_cache::{CacheCore, Caching};
use pstore_engine::{Engine, EngineError};
use pstore_format::Document;
use pstore_types::{LaneId, TenantId};
use std::collections::HashMap;
use std::sync::Mutex;

/// Epochs of history kept when a reap does not say. ⚠️ Deliberately generous: the cost of
/// keeping a graveyard entry is bytes in HEAD, and the cost of reaping one is a caller's
/// history.
const DEFAULT_RETENTION: u64 = 64;
use std::sync::Arc;
use types::Schema as SchemaOut;

mod auth;
pub use auth::{Auth, AuthError};
mod body;
mod boost;
mod diversify;
mod external;
mod facets;
mod highlight;
mod ingress;
pub use ingress::{IngressCounts, LimitsError, RequestLimits, RequestLimitsBuilder};
mod limit_per;
mod peer_http;
pub use peer_http::PeerTransport;
mod peer_tls;
mod peer_wire;
mod ref_new;
mod rerank;
#[cfg(test)]
#[path = "../tests/support/tls.rs"]
mod tls_for_tests;
pub use peer_tls::{PeerListenerConfig, PeerTls};
mod peers;
mod replication;
mod session;
mod typed;
mod types;
pub use peers::{GossipConfig, PeerConfig, PeerGossip, peer_list_of};
pub use replication::{ReplicationPolicy, Worker, WorkerTick, run_replication};
pub use types::{
    Cost, Duties, Duty, ErrorBody, FoldResponse, GcResponse, IdempotencyKeyResponse, IndexList,
    IndexProgress, IndexSummary, ListParams, MultiQueryMeta, MultiQueryResponse, QueryMeta,
    QueryRequest, QueryResponse, ReadOnlyResponse, ResultRow, Schema, TextIn, WarmMeta,
    WarmResponse, WriteRequest, WriteResponse,
};

/// Everything a running server leaves to whoever deploys it.
///
/// ⚠️ **Each of these is a way to lose data or to be breached**, which is why they are named
/// rather than left implicit. A server that folds nothing accumulates WAL bundles forever and
/// answers every query from the freshness layer; a server that reaps nothing keeps every
/// buried segment; a server behind no TLS and no authentication is a bucket anyone can write.
pub const UNSCHEDULED: [Duty; 4] = [
    Duty {
        id: "fold",
        instead: "a scheduled fold runs inside each server (M9i.1), but only for tenants that \
                  server wrote; a tenant whose only writer died or restarted is invisible to \
                  every other process until someone calls POST /v1/admin/fold for it",
    },
    Duty {
        id: "reap",
        instead: "a scheduled reap runs inside each server (M18), for the tenants that server \
                  committed to; a tenant nothing commits to after a restart keeps its buried \
                  objects until something does, or until someone calls POST /v1/admin/gc",
    },
    Duty {
        id: "tls",
        instead: "terminate public HTTP TLS in a trusted ingress; configure a dedicated \
                  deployment CA and mTLS identity for HTTP/2 and HTTP/3 peer listeners",
    },
    Duty {
        id: "auth",
        instead: "configure PSTORE_AUTH_CONFIG and issue scoped credentials; distribute and \
                  rotate signing keys through the deployment's secret facility",
    },
];

/// Why a server refused to start.
///
/// ⚠️ **A startup refusal, not a request-time one.** M7a made `admits_durable_writes()`
/// public for exactly this caller: a backend whose recorded profile cannot fence must fail
/// loudly rather than corrupt silently, and the guards inside the engine are at the *flush*.
/// A **batched** write reaches only `check_storable`, so without this door a write to a
/// divergent backend answers `200` and is refused minutes later, if ever.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartupError {
    /// The backend's recorded profile does not admit durable writes.
    #[error("backend {backend} cannot fence: cas={cas:?}, create_if_absent={create:?}")]
    CannotFence {
        /// What the backend calls itself.
        backend: String,
        /// Its compare-and-swap support.
        cas: pstore_blob::Support,
        /// Its create-if-absent support.
        create: pstore_blob::Support,
    },
}

/// A tenant's engine: its view of the store, read through the process's cache (M20).
type Tenant<S> = Engine<Caching<TenantView<S>>>;

/// One process's serving state: the store, the lane it writes on, and one engine per tenant.
///
/// ⚠️ **One `Engine` per tenant, held while it holds anything a restart would lose.** The
/// engine owns the memtable that makes a write visible before it is folded, so it cannot be
/// rebuilt per request -- and evicting one with unflushed rows would discard acknowledged
/// writes. M7c left the registry unbounded rather than decide that; M26 bounds it at
/// `PSTORE_ENGINES`, dropping only engines that are idle (`Engine::is_idle`), held by no
/// request, and owed no fold or reap.
#[derive(Debug)]
pub struct Api<S> {
    auth: Auth,
    /// Public request admission (M128): one per `Api`, shared by every router it builds.
    ingress: Arc<ingress::Ingress>,
    store: Accounted<S>,
    lane: LaneId,
    /// The read cache every tenant's view shares, or `None` for an uncached server (M20).
    cache: Option<Arc<CacheCore>>,
    /// Each tenant's engine and when a request last asked for it (M26).
    engines: tokio::sync::Mutex<HashMap<TenantId, Slot<S>>>,
    /// The registry's cap, `0` for none, and the state of its eviction loop (M26).
    registry: Mutex<Registry>,
    /// What `GET /metrics` reports about this process's own traffic.
    ///
    /// ⚠️ **On the `Api`, not in a static.** A process-global counter would make the refusal
    /// assertions untestable under a parallel test binary — the same argument `Config` already
    /// makes about reading the environment.
    http: Mutex<Http>,
    /// Per tenant, when its next scheduled fold may run after a failure, and the delay that
    /// set it (M9i.1).
    backoff: Mutex<HashMap<TenantId, (tokio::time::Instant, std::time::Duration)>>,
    /// Tenants a refused `strong` read asked a fold of (M9i.2). A set: many refusals, one fold.
    requested: Mutex<std::collections::HashSet<TenantId>>,
    /// Per tenant, when its next scheduled reap may run after a failure, and the delay that
    /// set it (M18).
    reap_backoff: Mutex<HashMap<TenantId, (tokio::time::Instant, std::time::Duration)>>,
    /// Replication (M22): its policy and named sources, the register of tenants with running
    /// replications, and what this process's worker holds.
    replicating: replication::Replicating<S>,
    /// The servers a query's vector legs may run on (M54), or `None` for a single server.
    peers: Mutex<Option<Arc<peers::Cluster>>>,
    peer_pool: Mutex<Option<Arc<peer_http::Pool>>>,
    peer_limits: Arc<peer_http::Limits>,
    /// Parts of other servers' queries this one ran (M54).
    parts_served: std::sync::atomic::AtomicU64,
    parts_completed: [[std::sync::atomic::AtomicU64; 3]; 2],
    /// Parts held between their two phases (M55).
    held: Mutex<peers::Holder>,
    /// Index parameters every engine is built with, when set (M54's tests cluster small
    /// segments with them).
    index_params: Mutex<Option<pstore_index::cluster::Params>>,
    /// The prefixes of customers' objects tenants may register as external indexes (M86), or
    /// `None` for off, the default. Read when a tenant's engine is built.
    external_prefixes: Mutex<Option<Vec<String>>>,
}

/// One tenant's engine in the registry (M26).
#[derive(Debug)]
struct Slot<S> {
    engine: Arc<Tenant<S>>,
    used: tokio::time::Instant,
}

/// The registry's cap and its eviction loop's state (M26).
#[derive(Debug, Default)]
struct Registry {
    /// `PSTORE_ENGINES`: past it, idle engines are dropped. `0` for unbounded.
    cap: usize,
    /// Whether a reap loop runs: only then does a reapable commit keep an engine.
    reaping: bool,
    /// `PSTORE_APPLIED_WRITES=1` (M129): every node of the deployment folds applied writes.
    applied_writes: bool,
    /// `PSTORE_APPLIED_WRITES=2` (M130): every node writes and reads outcome ids.
    affected_ids: bool,
    /// `PSTORE_APPLIED_WRITES=3` (M132): every node decides idempotency keys.
    idempotency: bool,
    /// `PSTORE_APPLIED_WRITES=4` (M137): every node reads an outcome's remaining section, so
    /// `allow_partial` by-filter writes are served.
    partial_filters: bool,
    /// The bound on one by-filter operation (M136), when not the engine's default.
    filter_rows: Option<usize>,
    /// `PSTORE_APPLIED_WRITES=5` (M139): `affected_ids` beside a by-filter operation.
    filter_ids: bool,
    /// How stale an engine's read of its lane may grow before a flush reads HEAD again (M39).
    lane_recheck: Option<std::time::Duration>,
    /// No scan before this, after one that freed nothing; and how long it waited.
    next_scan: Option<tokio::time::Instant>,
    backoff: std::time::Duration,
    evicted: u64,
    scans: u64,
}

/// What one eviction pass did (M26).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EvictTick {
    /// Whether it scanned the registry.
    pub scanned: bool,
    /// Engines it dropped.
    pub evicted: usize,
}

/// When a scheduled fold runs (M9i.1): D-39's triggers, size or age, never a fixed timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldPolicy {
    /// How often the loop looks. Looking costs no request.
    pub period: std::time::Duration,
    /// A tenant is due once its oldest unfolded batch is this old.
    pub age: std::time::Duration,
    /// A tenant is due once its unfolded bundles total this many bytes.
    pub bytes: u64,
}

impl Default for FoldPolicy {
    /// D-39's own triggers: an hour, or a mebibyte.
    fn default() -> Self {
        Self {
            period: std::time::Duration::from_secs(1),
            age: std::time::Duration::from_secs(3600),
            bytes: 1 << 20,
        }
    }
}

/// What one [`Api::fold_due`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FoldTick {
    /// Tenants whose fold committed.
    pub folded: usize,
    /// Tenants whose fold found nothing: another process had folded them.
    pub nothing: usize,
    /// Tenants whose fold failed; each now backs off.
    pub failed: usize,
    /// Due tenants skipped because they are still backing off.
    pub deferred: usize,
}

/// Folds at once, at most, per tick.
const FOLD_CONCURRENCY: usize = 4;

/// When a scheduled reap runs (M18): a tenant is due once a commit this process made there is
/// `age` old, and it is reaped through the highest such epoch. Decided from memory, never by
/// reading HEAD, so an idle tenant costs no request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcPolicy {
    /// How often the loop looks. Looking costs no request.
    pub period: std::time::Duration,
    /// How long a buried object is kept: `compaction.md`'s retention window.
    pub age: std::time::Duration,
}

impl Default for GcPolicy {
    /// `compaction.md`'s window: about an hour.
    fn default() -> Self {
        Self {
            period: std::time::Duration::from_secs(1),
            age: std::time::Duration::from_secs(3600),
        }
    }
}

/// What one [`Api::reap_due`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReapTick {
    /// Tenants reaped, whether or not the reap found anything.
    pub reaped: usize,
    /// Objects those reaps deleted.
    pub objects: usize,
    /// Tenants whose reap failed; each now backs off, and keeps what made it due.
    pub failed: usize,
    /// Due tenants skipped because they are still backing off.
    pub deferred: usize,
}

/// Per-route and per-refusal counters. ⚠️ **No tenant dimension**: 1M tenants × four request
/// classes is four million series, which is how a metrics endpoint takes down the thing it
/// observes. A tenant's own numbers go to that tenant, in every response it gets.
#[derive(Debug, Default)]
struct Http {
    /// `(route, status) -> count`.
    requests: std::collections::BTreeMap<(String, u16), u64>,
    /// `code -> count`, for the refusals the error table names.
    refusals: std::collections::BTreeMap<&'static str, u64>,
    /// `outcome -> count` for scheduled folds (M9i.1).
    folds: std::collections::BTreeMap<&'static str, u64>,
}

impl<S: BlobStore + 'static> Api<S> {
    /// Builds a server, or refuses because the backend cannot fence.
    ///
    /// # Errors
    /// [`StartupError::CannotFence`] when the recorded profile's `cas` or `create_if_absent`
    /// is anything but `Supported`. **Zero requests**: it reads the profile the store already
    /// carries and probes nothing.
    pub fn new(store: Accounted<S>, lane: LaneId) -> Result<Arc<Self>, StartupError> {
        Self::build(store, lane, None, Auth::closed(), RequestLimits::default())
    }

    /// [`Api::new`], reading every tenant's segments through `cache` (M20).
    ///
    /// ⚠️ The cache wraps each tenant's view of the accounted store, so a hit never reaches
    /// the accounting and is never billed.
    ///
    /// # Errors
    /// As [`Api::new`].
    pub fn with_cache(
        store: Accounted<S>,
        lane: LaneId,
        cache: Arc<CacheCore>,
    ) -> Result<Arc<Self>, StartupError> {
        Self::build(
            store,
            lane,
            Some(cache),
            Auth::closed(),
            RequestLimits::default(),
        )
    }

    /// Builds an HTTP server with an explicit authentication policy.
    ///
    /// # Errors
    /// As [`Api::new`].
    pub fn with_auth(
        store: Accounted<S>,
        lane: LaneId,
        auth: Auth,
    ) -> Result<Arc<Self>, StartupError> {
        Self::build(store, lane, None, auth, RequestLimits::default())
    }

    /// [`Api::with_auth`] with a shared read cache.
    ///
    /// # Errors
    /// As [`Api::new`].
    pub fn with_cache_and_auth(
        store: Accounted<S>,
        lane: LaneId,
        cache: Arc<CacheCore>,
        auth: Auth,
    ) -> Result<Arc<Self>, StartupError> {
        Self::build(store, lane, Some(cache), auth, RequestLimits::default())
    }

    /// Explicitly anonymous HTTP for private embeddings and test fixtures.
    ///
    /// # Errors
    /// As [`Api::new`].
    pub fn unsecured(store: Accounted<S>, lane: LaneId) -> Result<Arc<Self>, StartupError> {
        Self::build(
            store,
            lane,
            None,
            Auth::unsecured(),
            RequestLimits::default(),
        )
    }

    /// [`Api::unsecured`] with a shared read cache.
    ///
    /// # Errors
    /// As [`Api::new`].
    pub fn with_cache_unsecured(
        store: Accounted<S>,
        lane: LaneId,
        cache: Arc<CacheCore>,
    ) -> Result<Arc<Self>, StartupError> {
        Self::build(
            store,
            lane,
            Some(cache),
            Auth::unsecured(),
            RequestLimits::default(),
        )
    }

    /// An HTTP server with an explicit authentication policy, an optional read cache, and
    /// public request limits other than the defaults (M128).
    ///
    /// # Errors
    /// As [`Api::new`].
    pub fn with_limits(
        store: Accounted<S>,
        lane: LaneId,
        cache: Option<Arc<CacheCore>>,
        auth: Auth,
        limits: RequestLimits,
    ) -> Result<Arc<Self>, StartupError> {
        Self::build(store, lane, cache, auth, limits)
    }

    /// The public request limits every router of this server admits under (M128).
    pub fn request_limits(&self) -> RequestLimits {
        *self.ingress.limits()
    }

    /// What public admission holds right now (M128's tests and measurements).
    #[doc(hidden)]
    pub fn ingress_counts(&self) -> IngressCounts {
        self.ingress.counts()
    }

    fn build(
        store: Accounted<S>,
        lane: LaneId,
        cache: Option<Arc<CacheCore>>,
        auth: Auth,
        limits: RequestLimits,
    ) -> Result<Arc<Self>, StartupError> {
        // ⚠️ `Accounted` is not itself a `BlobStore` — a tenant view is — so the profile is
        // read through one. It is the same underlying backend's, and reading it costs nothing.
        let probe = store.as_tenant(TenantId(0));
        let caps = probe.capabilities();
        if !caps.admits_durable_writes() {
            return Err(StartupError::CannotFence {
                backend: caps.backend.clone(),
                cas: caps.cas.clone(),
                create: caps.create_if_absent.clone(),
            });
        }
        Ok(Arc::new(Self {
            auth,
            ingress: Arc::new(ingress::Ingress::new(limits)),
            store,
            lane,
            cache,
            engines: tokio::sync::Mutex::new(HashMap::new()),
            registry: Mutex::new(Registry::default()),
            http: Mutex::new(Http::default()),
            backoff: Mutex::new(HashMap::new()),
            requested: Mutex::new(std::collections::HashSet::new()),
            reap_backoff: Mutex::new(HashMap::new()),
            replicating: replication::Replicating::default(),
            peers: Mutex::new(None),
            peer_pool: Mutex::new(None),
            peer_limits: Arc::new(peer_http::Limits::default()),
            parts_served: std::sync::atomic::AtomicU64::new(0),
            parts_completed: std::array::from_fn(|_| {
                std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0))
            }),
            held: Mutex::new(peers::Holder::default()),
            index_params: Mutex::new(None),
            external_prefixes: Mutex::new(None),
        }))
    }

    /// Passes `result` through, first marking `tenant` for a fold if it is a `not_folded`
    /// refusal (M9i.2). The fold loop takes the mark; nothing is folded here, on the query's
    /// path.
    async fn requesting<T>(
        &self,
        tenant: TenantId,
        result: Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        if let Err(e) = &result
            && e.code == "not_folded"
        {
            self.requested
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(tenant);
        }
        result
    }

    /// Folds every tenant this process holds that `policy` says is due (M9i.1).
    ///
    /// ⚠️ **Decided from memory, so an idle tenant costs no request**: a tenant is due only if
    /// this process flushed a batch nobody is yet known to have folded, and it is old or large
    /// enough. The engines are snapshotted and the map's lock released before any fold, so a
    /// request never waits behind one. A failed fold backs off, doubling from `period` up to
    /// `age`, so a tenant that cannot fold is not retried every tick.
    pub async fn fold_due(&self, policy: &FoldPolicy) -> FoldTick {
        self.fold_tick(policy, &std::sync::atomic::AtomicBool::new(false))
            .await
    }

    /// Reaps every tenant a scheduled reap is due for (M18): one whose engine committed something
    /// at least `policy.age` ago, through the highest such epoch.
    ///
    /// ⚠️ **Decided from memory**, as [`Self::fold_due`] is: a tenant this process has committed
    /// nothing to, or nothing old enough, costs no request. A failed reap backs off, doubling
    /// from `period` up to `age`, and keeps the records that made it due.
    pub async fn reap_due(&self, policy: &GcPolicy) -> ReapTick {
        self.reap_tick(policy, &std::sync::atomic::AtomicBool::new(false))
            .await
    }

    /// [`Self::reap_due`], starting no tenant's reap once `halt` is set.
    async fn reap_tick(&self, policy: &GcPolicy, halt: &std::sync::atomic::AtomicBool) -> ReapTick {
        use futures_util::StreamExt;
        let engines: Vec<(TenantId, Arc<Tenant<S>>)> = self
            .engines
            .lock()
            .await
            .iter()
            .map(|(t, s)| (*t, Arc::clone(&s.engine)))
            .collect();
        let now = tokio::time::Instant::now();
        let mut tick = ReapTick::default();
        let mut due = Vec::new();
        for (tenant, engine) in engines {
            let Some(horizon) = engine.reap_due(policy.age) else {
                continue;
            };
            let waiting = self
                .reap_backoff
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&tenant)
                .is_some_and(|(until, _)| now < *until);
            if waiting {
                tick.deferred += 1;
                continue;
            }
            due.push((tenant, engine, horizon));
        }
        let results: Vec<_> = futures_util::stream::iter(due)
            .map(|(tenant, engine, horizon)| async move {
                if halt.load(std::sync::atomic::Ordering::SeqCst) {
                    return None;
                }
                Some((tenant, engine.gc_through(horizon).await))
            })
            .buffer_unordered(FOLD_CONCURRENCY)
            .filter_map(std::future::ready)
            .collect()
            .await;
        let mut backoff = self
            .reap_backoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (tenant, outcome) in results {
            match outcome {
                Ok(n) => {
                    backoff.remove(&tenant);
                    tick.reaped += 1;
                    tick.objects += n;
                }
                Err(_) => {
                    tick.failed += 1;
                    let delay = backoff
                        .get(&tenant)
                        .map_or(policy.period, |(_, d)| d.saturating_mul(2))
                        .min(policy.age);
                    let now = tokio::time::Instant::now();
                    let until = now
                        .checked_add(delay)
                        .unwrap_or_else(|| now + std::time::Duration::from_secs(10 * 365 * 86_400));
                    backoff.insert(tenant, (until, delay));
                }
            }
        }
        tick
    }

    /// [`Self::fold_due`], starting no tenant's fold once `halt` is set: the ones already in
    /// flight finish, the rest are left due for the next process to run.
    async fn fold_tick(
        &self,
        policy: &FoldPolicy,
        halt: &std::sync::atomic::AtomicBool,
    ) -> FoldTick {
        use futures_util::StreamExt;
        let engines: Vec<(TenantId, Arc<Tenant<S>>)> = self
            .engines
            .lock()
            .await
            .iter()
            .map(|(t, s)| (*t, Arc::clone(&s.engine)))
            .collect();
        let now = tokio::time::Instant::now();
        let mut tick = FoldTick::default();
        let mut due = Vec::new();
        for (tenant, engine) in engines {
            // A refused `strong` read asked for this tenant (M9i.2), whatever it holds.
            let requested = self
                .requested
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&tenant);
            let old_or_large = engine.unfolded().is_some_and(|u| {
                now.duration_since(u.oldest) >= policy.age || u.bytes >= policy.bytes
            });
            if !old_or_large && !requested {
                continue;
            }
            let waiting = self
                .backoff
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&tenant)
                .is_some_and(|(until, _)| now < *until);
            if waiting {
                tick.deferred += 1;
                continue;
            }
            due.push((tenant, engine));
        }
        let results: Vec<_> = futures_util::stream::iter(due)
            .map(|(tenant, engine)| async move {
                if halt.load(std::sync::atomic::Ordering::SeqCst) {
                    return None;
                }
                // The mark is taken as the fold starts, and put back below if it fails.
                let requested = self
                    .requested
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&tenant);
                // M26: the engine travels with its result, held until the mark is put back --
                // an engine no request holds and no mark owes is one eviction may drop.
                let outcome = engine.fold_committed().await;
                Some((tenant, requested, outcome, engine))
            })
            .buffer_unordered(FOLD_CONCURRENCY)
            .filter_map(std::future::ready)
            .collect()
            .await;
        let mut backoff = self
            .backoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (tenant, requested, outcome, _held) in results {
            if requested && outcome.is_err() {
                self.requested
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(tenant);
            }
            match outcome {
                Ok(committed) => {
                    backoff.remove(&tenant);
                    if committed {
                        tick.folded += 1;
                    } else {
                        tick.nothing += 1;
                    }
                }
                Err(_) => {
                    tick.failed += 1;
                    let delay = backoff
                        .get(&tenant)
                        .map_or(policy.period, |(_, d)| d.saturating_mul(2))
                        .min(policy.age);
                    // `checked_add`: an absurd configured period or age must not panic the loop
                    // dead. Past what an `Instant` holds, a decade is as good as forever.
                    let now = tokio::time::Instant::now();
                    let until = now
                        .checked_add(delay)
                        .unwrap_or_else(|| now + std::time::Duration::from_secs(10 * 365 * 86_400));
                    backoff.insert(tenant, (until, delay));
                }
            }
        }
        drop(backoff);
        let mut http = self
            .http
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (outcome, n) in [
            ("folded", tick.folded),
            ("nothing", tick.nothing),
            ("failed", tick.failed),
        ] {
            *http.folds.entry(outcome).or_default() += n as u64;
        }
        tick
    }

    /// The routes. `api-design.md`'s surface, minus everything M7c defers.
    pub fn router(self: Arc<Self>) -> Router {
        use auth::Scope::{Admin, List, Read, Write};
        auth::ScopedRouter::new(Arc::clone(&self.ingress))
            .route("/v1/indexes", List.on(get(list_indexes::<S>)))
            .route("/v1/indexes/{index}", Read.on(get(index_summary::<S>)))
            .route(
                "/v1/indexes/{index}",
                Write.on(axum::routing::delete(delete_index::<S>)
                    .put(create_index::<S>)
                    .patch(patch_index::<S>)),
            )
            .route(
                "/v1/indexes/{index}/documents",
                Write.on(put(write_documents::<S>)),
            )
            .route("/v1/indexes/{index}/query", Read.on(post(query_index::<S>)))
            .route("/v1/indexes/{index}/warm", Read.on(post(warm_index::<S>)))
            // M132: a key that makes an applied write's retry safe.
            .route(
                "/v1/indexes/{index}/idempotency-keys",
                Write.on(post(issue_idempotency_key::<S>)),
            )
            // M86: an external index, over the customer's own object.
            .route(
                "/v1/indexes/{index}/external",
                Write.on(put(external::register::<S>).delete(external::drop_index::<S>)),
            )
            .route(
                "/v1/indexes/{index}/external/query",
                Read.on(post(external::query::<S>)),
            )
            // M92: a copy of another external index, codes moved and nothing rebuilt.
            .route(
                "/v1/indexes/{index}/external/copy",
                Write.on(put(external::copy::<S>)),
            )
            .route(
                "/v1/indexes/{index}/quarantine",
                Read.on(get(export_quarantine::<S>)),
            )
            .route(
                "/v1/indexes/{index}/quarantine",
                Write.on(axum::routing::delete(discard_quarantine::<S>)),
            )
            .route("/v1/replications", List.on(get(replication::list::<S>)))
            .route(
                "/v1/indexes/{index}/replication",
                Write.on(put(replication::create::<S>).delete(replication::cancel::<S>)),
            )
            .route(
                "/v1/indexes/{index}/replication",
                Read.on(get(replication::status::<S>)),
            )
            .route(
                "/v1/indexes/{index}/replication/pause",
                Write.on(post(replication::pause::<S>)),
            )
            .route(
                "/v1/indexes/{index}/replication/resume",
                Write.on(post(replication::resume::<S>)),
            )
            .route(
                "/v1/indexes/{index}/schema",
                Write.on(axum::routing::patch(patch_schema)),
            )
            .route("/v1/admin/fold", Admin.on(post(fold_tenant::<S>)))
            .route("/v1/admin/gc", Admin.on(post(gc_tenant::<S>)))
            .route("/v1/admin/duties", Admin.on(get(duties)))
            .route("/metrics", Admin.on(get(metrics::<S>)))
            .finish()
            // M128: the ceiling axum's own extractors held implicitly, now the typed limit.
            .layer(axum::extract::DefaultBodyLimit::max(
                self.ingress.limits().body_bytes(),
            ))
            .layer(axum::middleware::from_fn_with_state(
                self.auth.clone(),
                auth::authenticate,
            ))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&self),
                count_request::<S>,
            ))
            .with_state(self)
    }

    /// The engine for `tenant`, built on first use.
    async fn engine(&self, tenant: TenantId) -> Arc<Tenant<S>> {
        let (recheck, keys, filter_rows) = {
            let r = self.registry_mut();
            (r.lane_recheck, r.idempotency, r.filter_rows)
        };
        let params = *self
            .index_params
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut engines = self.engines.lock().await;
        let slot = engines.entry(tenant).or_insert_with(|| {
            // Read only for an engine being built (M119): every request copied the list.
            let external = self
                .external_prefixes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let engine = Engine::new(
                Arc::new(Caching::over(
                    Arc::new(self.store.as_tenant(tenant)),
                    self.cache.clone(),
                )),
                tenant,
                self.lane,
            );
            let engine = match params {
                Some(p) => engine.with_index_params(p),
                None => engine,
            };
            let engine = match filter_rows {
                Some(rows) => engine.with_filter_rows(rows),
                None => engine,
            };
            engine.set_idempotency(keys);
            let engine = match &external {
                Some(prefixes) => engine.with_external_sources(prefixes.clone()),
                None => engine,
            };
            Slot {
                engine: Arc::new(match recheck {
                    Some(within) => engine.with_lane_recheck(within),
                    None => engine,
                }),
                used: tokio::time::Instant::now(),
            }
        });
        // M26: the only thing the registry's cap adds to a request.
        slot.used = tokio::time::Instant::now();
        Arc::clone(&slot.engine)
    }

    /// The engine for `tenant`, held as a request would hold it (M26's tests).
    #[doc(hidden)]
    pub async fn hold_engine_for_test(&self, tenant: TenantId) -> Arc<Tenant<S>> {
        self.engine(tenant).await
    }

    /// Splits each query's vector legs across `peers` (M54); `None` answers every query here.
    ///
    /// # Errors
    /// No HTTP client can be built for the peers.
    pub fn set_peers(&self, peers: Option<PeerConfig>) -> Result<(), ConfigError> {
        let cluster = peers
            .map(|config| {
                let pool = self.peer_pool(config.timeout)?;
                peers::Cluster::new(config, pool).map(Arc::new)
            })
            .transpose()?;
        if let Some(pool) = self
            .peer_pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            pool.retain(
                &cluster
                    .as_ref()
                    .map(|c| c.list().servers.clone())
                    .unwrap_or_default(),
            );
        }
        *self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cluster;
        Ok(())
    }

    fn peer_pool(&self, timeout: std::time::Duration) -> Result<Arc<peer_http::Pool>, ConfigError> {
        let (tls, mode) = self.auth.peer_tls().ok_or_else(|| {
            ConfigError::Peers(
                "PSTORE_PEER_TLS",
                String::new(),
                "peer mTLS identity is required",
            )
        })?;
        let mut pool = self
            .peer_pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pool) = pool.as_ref() {
            return Ok(pool.clone());
        }
        let created =
            peer_http::Pool::with_limits(tls.clone(), *mode, timeout, self.peer_limits.clone())
                .map_err(|_| {
                    ConfigError::Peers(
                        "PSTORE_PEER_TLS",
                        String::new(),
                        "valid TLS peer transport and timeout required",
                    )
                })?;
        *pool = Some(created.clone());
        Ok(created)
    }

    /// Builds every engine from now on with `params` (M54's tests: small clustered segments).
    #[doc(hidden)]
    pub fn set_index_params_for_test(&self, params: pstore_index::cluster::Params) {
        *self
            .index_params
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(params);
    }

    /// The parts held between their phases (M55).
    fn holder(&self) -> std::sync::MutexGuard<'_, peers::Holder> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes the peer list from a SWIM membership this server runs (M56): its member declares
    /// `config.url` as its zone, and once a period the list becomes every member whose zone is
    /// a server's URL. Until the first refresh the list is this server alone.
    ///
    /// The returned handle keeps the member and the refresh running; dropping it, or
    /// [`PeerGossip::stop`], ends both.
    ///
    /// # Errors
    /// Peers already set, by a list or an earlier call (code review: a second cluster would
    /// reset the counters and orphan the first refresh); no HTTP client for the peers; or a
    /// member that cannot start (its address in use).
    pub async fn join_peers(&self, config: GossipConfig) -> Result<PeerGossip, ConfigError> {
        if self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            return Err(ConfigError::Peers(
                "PSTORE_PEER_GOSSIP_ADDR",
                config.listen,
                "this server's peers are set already: a static list or gossip, once",
            ));
        }
        let pool = self.peer_pool(config.timeout)?;
        let cluster = Arc::new(peers::Cluster::new(
            PeerConfig {
                servers: vec![config.url.clone()],
                me: 0,
                timeout: config.timeout,
            },
            pool,
        )?);
        let gossip = PeerGossip::start(&config, Arc::clone(&cluster)).await?;
        *self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cluster);
        Ok(gossip)
    }

    /// The peer list a query would take now (M56's tests); empty when unpeered.
    #[doc(hidden)]
    #[must_use]
    pub fn peer_list(&self) -> Vec<String> {
        self.peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|c| c.list().servers.clone())
            .unwrap_or_default()
    }

    /// This query's view of the peers, when there are any (M54): one snapshot of the list,
    /// which every share of the query then reads (M56).
    fn query_peers(&self, tenant: TenantId, req: &QueryRequest) -> Option<peers::QueryPeers> {
        let cluster = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        Some(peers::QueryPeers {
            list: cluster.list(),
            cluster,
            tenant,
            filters: req.filters.clone(),
        })
    }

    /// Allows tenants to register external indexes over their own objects under `prefixes`
    /// (M86, `PSTORE_EXTERNAL_SOURCES=allow`). Off until called, and meant to be called once at
    /// startup: an engine already built keeps what it was built with.
    pub fn allow_external_sources(&self, prefixes: Vec<String>) {
        *self
            .external_prefixes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(prefixes);
    }

    /// Caps the registry at `cap` engines, `0` for unbounded (M26, `PSTORE_ENGINES`): past it,
    /// the eviction loop drops the least recently used idle engines.
    pub fn limit_engines(&self, cap: usize) {
        self.registry_mut().cap = cap;
    }

    /// Every engine built from now on re-reads HEAD on a flush whose last read of its lane is
    /// older than `within` (M39): half of `PSTORE_GC_AGE_S`, which `Config::lane_recheck`
    /// gives, whether or not this process reaps -- another server reaping the tenant reaps
    /// this lane's bundles too.
    pub fn recheck_lanes_within(&self, within: std::time::Duration) {
        self.registry_mut().lane_recheck = Some(within);
    }

    /// Whether a reap loop runs (M26): only then does a reapable commit keep an engine, since
    /// nothing else would ever collect it. [`serve_folding`] sets it.
    pub fn set_reaping(&self, on: bool) {
        self.registry_mut().reaping = on;
    }

    /// Whether `durability: "applied"` is served (M129). ⚠️ **An operator's statement that
    /// every node runs this release**: an older fold would seal the `$req` stamp as an
    /// attribute and drop HEAD's outcome list.
    pub fn set_applied_writes(&self, on: bool) {
        self.registry_mut().applied_writes = on;
    }

    /// Whether an applied write may ask for `affected_ids` (M130). ⚠️ **An operator's
    /// statement that every node runs M130**: an M129 waiter refuses an outcome object with
    /// an ids section.
    pub fn set_affected_ids(&self, on: bool) {
        self.registry_mut().affected_ids = on;
    }

    /// Whether an applied write may carry an `idempotency_key` (M132), and every engine
    /// initialises HEAD's `keys_since`. ⚠️ **An operator's statement that every node runs
    /// M132**: an older fold applies every attempt of a key, and an M130 waiter refuses an
    /// outcome object with a keys section.
    pub fn set_idempotency(&self, on: bool) {
        self.registry_mut().idempotency = on;
    }

    /// Serves `delete_by_filter_allow_partial` and `patch_by_filter_allow_partial` (M137):
    /// `PSTORE_APPLIED_WRITES=4`. ⚠️ **An operator's statement that every node runs M137**:
    /// an M136 node refuses an outcome object with a remaining section, and so does its
    /// fold whenever it decides a key.
    pub fn set_partial_filters(&self, on: bool) {
        self.registry_mut().partial_filters = on;
    }

    /// Serves `affected_ids` beside a by-filter operation (M139): `PSTORE_APPLIED_WRITES=5`.
    /// ⚠️ **An operator's statement that every node runs M139**: an older fold ignores the
    /// operation's own bound, and would list up to the engine's bound of ids.
    pub fn set_filter_ids(&self, on: bool) {
        self.registry_mut().filter_ids = on;
    }

    /// Builds every engine from now on with by-filter operations bounded at `rows` (M136's
    /// tests, and M137's).
    #[doc(hidden)]
    pub fn set_filter_rows_for_test(&self, rows: usize) {
        self.registry_mut().filter_rows = Some(rows);
    }

    /// Engines the registry holds.
    pub async fn engines(&self) -> usize {
        self.engines.lock().await.len()
    }

    /// Engines evicted, and eviction scans made, since the process started.
    #[must_use]
    pub fn eviction_counts(&self) -> (u64, u64) {
        let r = self.registry_mut();
        (r.evicted, r.scans)
    }

    fn registry_mut(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One eviction pass (M26): past the cap, drops idle engines no request holds, least
    /// recently used first, down to `cap - cap / 10`. Memory only: no blob request.
    ///
    /// ⚠️ **Idle** is what a restart of this tenant alone would lose nothing to, and **held** --
    /// an `Arc` beyond the registry's own -- is a request in flight: dropping that one would let
    /// the next request build a second engine on the same lane in one process. Both are read
    /// under the registry's lock, which every way of reaching an engine takes.
    #[doc(hidden)]
    pub async fn evict_tick(&self) -> EvictTick {
        let now = tokio::time::Instant::now();
        let (cap, reaping) = {
            let r = self.registry_mut();
            if r.cap == 0 || r.next_scan.is_some_and(|at| now < at) {
                return EvictTick::default();
            }
            (r.cap, r.reaping)
        };
        let mut engines = self.engines.lock().await;
        if engines.len() <= cap {
            return EvictTick::default();
        }
        let target = cap - cap / 10;
        let mut idle: Vec<(tokio::time::Instant, TenantId)> = Vec::new();
        let mut held_idle = false;
        // The tenants this server owes a fold a refused `strong` read asked for (M9i.2): the one
        // thing it owes that its engine does not hold. Locked once for the pass (M119), not once
        // an engine while every request waits on `engines`; nothing takes the two in the other
        // order.
        //
        // ⚠️ **A backoff needs no check of its own** (mutation sweep): a fold that failed keeps
        // its own rows unfolded, or its request marked again, and a reap that failed keeps its
        // reapable records -- each of which keeps the tenant already.
        let owed = self
            .requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (t, s) in engines.iter() {
            if !s.engine.is_idle(reaping) || owed.contains(t) {
                continue;
            }
            if Arc::strong_count(&s.engine) > 1 {
                held_idle = true;
            } else {
                idle.push((s.used, *t));
            }
        }
        drop(owed);
        idle.sort_unstable();
        let n = (engines.len() - target).min(idle.len());
        for (_, t) in idle.iter().take(n) {
            engines.remove(t);
        }
        drop(engines);
        let mut r = self.registry_mut();
        r.scans += 1;
        r.evicted += n as u64;
        // ⚠️ A pass that freed nothing backs off -- unless it found an idle engine held, which
        // another tick's snapshot may have been holding a moment.
        if n == 0 && !held_idle {
            r.backoff = (r.backoff * 2).clamp(
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(60),
            );
            r.next_scan = Some(now + r.backoff);
        } else {
            r.backoff = std::time::Duration::ZERO;
            r.next_scan = None;
        }
        EvictTick {
            scanned: true,
            evicted: n,
        }
    }

    /// What `tenant` has spent so far, as the pair every handler diffs.
    ///
    /// ⚠️ **Monotone and per-tenant**, because that is what `Accounted` keeps. A handler's
    /// `cost` is therefore this snapshot subtracted from the one after, which is exact for a
    /// tenant with one request in flight and **cross-attributes under concurrency**. Stated
    /// rather than implied: making it exact needs a request-scoped counter in `pstore-blob`,
    /// which is a change to the layer every crate reads.
    fn spend(&self, tenant: TenantId) -> Spend {
        Spend {
            reads: self.store.count(tenant, OpClass::Read),
            writes: self.store.count(tenant, OpClass::Write),
            lists: self.store.count(tenant, OpClass::List),
            read_bytes: self.store.bytes(tenant, OpClass::Read),
            write_bytes: self.store.bytes(tenant, OpClass::Write),
        }
    }
}

/// A snapshot of one tenant's counters. Differences of these are what a response reports.
#[derive(Debug, Clone, Copy)]
struct Spend {
    reads: u64,
    writes: u64,
    lists: u64,
    read_bytes: u64,
    write_bytes: u64,
}

impl Spend {
    /// This request's own cost: `self` is taken after, `before` before.
    fn since(self, before: Self) -> Cost {
        Cost {
            blob_reads: self.reads.saturating_sub(before.reads),
            blob_writes: self.writes.saturating_sub(before.writes),
            blob_lists: self.lists.saturating_sub(before.lists),
            bytes_read: self.read_bytes.saturating_sub(before.read_bytes),
            bytes_written: self.write_bytes.saturating_sub(before.write_bytes),
        }
    }
}

/// A refusal the client can act on: a status, a stable code, and whether retrying may help.
#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    retryable: bool,
    /// Seconds for a `Retry-After` header, when the refusal names one (M9i.2).
    retry_after: Option<u64>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            // ⚠️ **Derived from the status, never passed**, so a handler cannot mark a `400`
            // retryable by accident and no two rows of the table can disagree about the same
            // status. `429` and `5xx` are the retryable classes; a request the client got
            // wrong will be just as wrong the second time.
            retryable: status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS,
            retry_after: None,
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    fn bad_session(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_session", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = self.code;
        let retry_after = self.retry_after;
        let body = ErrorBody::new(self.code, self.message, self.retryable);
        let mut res = (self.status, axum::Json(body)).into_response();
        if code == "unauthenticated" {
            res.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static("Bearer"),
            );
        }
        if let Some(secs) = retry_after {
            res.headers_mut()
                .insert(axum::http::header::RETRY_AFTER, secs.into());
        }
        res.extensions_mut().insert(RefusalCode(code));
        res
    }
}

/// The error table, as code.
///
/// ⚠️ `EngineError::Lost` and `Contended` are **409**, not 500: another writer landed, and
/// the client's remedy is to re-read and retry — which is a decision for the caller, exactly
/// as `Congested` refuses to retry a CAS on its behalf.
impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::Lost | EngineError::Contended => Self::new(
                StatusCode::CONFLICT,
                "version_conflict",
                "another writer landed first; re-read and retry",
            ),
            // ⚠️ **A client error, answered as one.** M7c's spec deferred this row because
            // the engine collapsed it into a string and a table that matches on messages is
            // a table no mutation can pin. The typed variant is what made the row possible.
            // ⚠️ The client wrote rows the index cannot hold. Without this arm it falls
            // through to `500 internal` -- the exact defect M7c's typed `DimensionMismatch`
            // was added to fix, repeated one milestone later, which is why M7d's spec made
            // the error-table arm an acceptance criterion of its own.
            // ⚠️ A caller asking for an epoch outside the window asked wrongly, and can tell
            // from the message which bound it crossed. Without this arm it is a `500`.
            // M9i.2: a strong read that met another process's unfolded write. Retryable once a
            // fold commits it, which the handler asks for.
            // M17: an operator's misconfiguration, not the client's -- two processes given
            // one lane. Loud, and named, so the fix is in the message.
            EngineError::LaneTaken { .. } => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "lane_taken",
                e.to_string(),
            ),
            EngineError::NotFolded => Self {
                retry_after: Some(1),
                ..Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "not_folded",
                    format!("{e} -- a fold has been requested; retry"),
                )
            },
            EngineError::TimeTravel(_) => Self::new(
                StatusCode::BAD_REQUEST,
                "time_travel_horizon",
                e.to_string(),
            ),
            EngineError::SchemaConflict { .. } => {
                Self::new(StatusCode::BAD_REQUEST, "schema_conflict", e.to_string())
            }
            EngineError::DimensionMismatch { .. } | EngineError::NoVectors(_) => {
                Self::new(StatusCode::BAD_REQUEST, "schema_conflict", e.to_string())
            }
            // M61: a `multi` leg's field that no segment holds; no retry mends it.
            // M155: a text leg's field that no segment holds.
            EngineError::UnknownField(_) | EngineError::UnknownTextField(_) => {
                Self::new(StatusCode::BAD_REQUEST, "unknown_field", e.to_string())
            }
            EngineError::Unmeasurable(_) => Self::bad_request(e.to_string()),
            // M16: the client asked for a branch the engine will not make.
            EngineError::Refused(_) => Self::bad_request(e.to_string()),
            EngineError::IndexExists(_) => {
                Self::new(StatusCode::CONFLICT, "index_exists", e.to_string())
            }
            EngineError::IndexReadOnly(_) => {
                Self::new(StatusCode::CONFLICT, "index_read_only", e.to_string())
            }
            // M22: each replication rule by its own name.
            EngineError::Replica(r) => {
                use pstore_engine::ReplicaRefusal as R;
                let (status, code) = match &r {
                    R::IndexExists(_) => (StatusCode::CONFLICT, "index_exists"),
                    R::Exists(_) => (StatusCode::CONFLICT, "replication_exists"),
                    R::SourceNotFound(_) => (StatusCode::NOT_FOUND, "source_not_found"),
                    R::NotFound(_) => (StatusCode::NOT_FOUND, "replication_not_found"),
                    R::ReadOnly(_) => (StatusCode::CONFLICT, "replica_read_only"),
                    R::Active(_) => (StatusCode::CONFLICT, "replication_active"),
                    R::NoHistory(_) => (StatusCode::CONFLICT, "replica_no_history"),
                    R::Invalid(_) => (StatusCode::BAD_REQUEST, "bad_request"),
                };
                Self::new(status, code, r.to_string())
            }
            // M86: each external-index refusal by its own name.
            EngineError::External(r) => {
                use pstore_engine::ExternalRefusal as R;
                let (status, code) = match &r {
                    R::Off => (StatusCode::FORBIDDEN, "external_sources_off"),
                    R::Prefix(_) => (StatusCode::FORBIDDEN, "external_source_not_allowed"),
                    R::Exists(_) => (StatusCode::CONFLICT, "index_exists"),
                    R::NotFound(_) => (StatusCode::NOT_FOUND, "external_index_not_found"),
                    R::Changed => (StatusCode::CONFLICT, "external_source_changed"),
                    R::Source(_) => (StatusCode::BAD_REQUEST, "external_source_unreadable"),
                };
                Self::new(status, code, r.to_string())
            }
            EngineError::BackendCannotFence { .. } => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                e.to_string(),
            ),
            // ⚠️ A blob failure is the storage tier's, not the client's, and every one of
            // them is worth retrying: `Congested` has already spent its bounded retries on a
            // 503 before the error reaches here.
            EngineError::Blob(_) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                e.to_string(),
            ),
            other => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                other.to_string(),
            ),
        }
    }
}

/// Counts every response: its route, its status, and the refusal code it carried.
///
/// ⚠️ **The code travels in a response extension**, inserted by `ApiError::into_response`,
/// because middleware sees a status and a body and the stable code lives in neither. The
/// alternative is counting inside the error constructor, which needs a process-global.
async fn count_request<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // ⚠️ A request axum refuses before routing has no matched path, and inventing one would
    // mean a series per 404 URL — which is unbounded and attacker-controlled.
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or_else(|| "<unmatched>".to_owned(), |m| m.as_str().to_owned());
    let res = next.run(req).await;
    let status = res.status().as_u16();
    let code = res.extensions().get::<RefusalCode>().map(|c| c.0);
    let mut http = api
        .http
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *http.requests.entry((route, status)).or_default() += 1;
    if let Some(code) = code {
        *http.refusals.entry(code).or_default() += 1;
    }
    res
}

/// `GET /v1/admin/duties` — what this server does not do.
///
/// ⚠️ **Zero blob requests** — it serves a constant, so a scraper cannot turn an honesty
/// endpoint into a bill.
async fn duties() -> Response {
    axum::Json(Duties {
        duties: &UNSCHEDULED,
    })
    .into_response()
}

/// `GET /metrics` — Prometheus text, and **no tenant dimension**.
///
/// ⚠️ **Zero blob requests**: every number here is a process-local atomic or map. A metrics
/// endpoint that reads the store is a load generator pointed at the thing it measures.
async fn metrics<S: BlobStore + 'static>(State(api): State<Arc<Api<S>>>) -> Response {
    let mut out = String::new();
    // M26: the engine registry.
    let (evicted, _) = api.eviction_counts();
    out.push_str(&format!(
        "# HELP pstore_engines Engines the registry holds.\n# TYPE pstore_engines gauge\n\
         pstore_engines {}\n\
         # HELP pstore_engines_evicted_total Idle engines dropped past PSTORE_ENGINES.\n\
         # TYPE pstore_engines_evicted_total counter\npstore_engines_evicted_total {evicted}\n",
        api.engines().await
    ));
    out.push_str(
        "# HELP pstore_blob_requests_total Blob requests by class.
",
    );
    out.push_str(
        "# TYPE pstore_blob_requests_total counter
",
    );
    for (class, name) in [
        (OpClass::Read, "read"),
        (OpClass::Write, "write"),
        (OpClass::List, "list"),
        (OpClass::Delete, "delete"),
    ] {
        out.push_str(&format!(
            "pstore_blob_requests_total{{class=\"{name}\"}} {}\n",
            api.store.total(class)
        ));
    }
    let http = api
        .http
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    out.push_str("# HELP pstore_http_requests_total HTTP responses by route and status.\n");
    out.push_str("# TYPE pstore_http_requests_total counter\n");
    for ((route, status), n) in &http.requests {
        out.push_str(&format!(
            "pstore_http_requests_total{{route=\"{route}\",status=\"{status}\"}} {n}\n"
        ));
    }
    out.push_str("# HELP pstore_fold_total Scheduled folds by outcome (M9i.1).\n");
    out.push_str("# TYPE pstore_fold_total counter\n");
    for outcome in ["folded", "nothing", "failed"] {
        out.push_str(&format!(
            "pstore_fold_total{{outcome=\"{outcome}\"}} {}\n",
            http.folds.get(outcome).copied().unwrap_or(0)
        ));
    }
    // M54: parts of queries sent to peers, those that failed, and those run for peers. M56:
    // and how many servers the list holds now, this one included.
    let (sent, failed, servers) = api
        .peers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map_or((0, 0, 1), |c| {
            (
                c.sent.load(std::sync::atomic::Ordering::Relaxed),
                c.failed.load(std::sync::atomic::Ordering::Relaxed),
                c.list().servers.len(),
            )
        });
    out.push_str(&format!(
        "# HELP pstore_peer_servers Servers a query may split across now, this one included.\n\
         # TYPE pstore_peer_servers gauge\npstore_peer_servers {servers}\n"
    ));
    out.push_str("# HELP pstore_peer_parts_completed_total Successful peer phases by transport.\n# TYPE pstore_peer_parts_completed_total counter\n");
    let peer_stats = api
        .peer_pool
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|pool| pool.stats());
    for (name, value) in [
        (
            "pstore_peer_pool_entries",
            peer_stats.as_ref().map_or(0, |s| s.entries),
        ),
        (
            "pstore_peer_connections",
            peer_stats
                .as_ref()
                .map_or_else(|| api.peer_limits.connections(), |s| s.connections),
        ),
    ] {
        out.push_str(&format!("# HELP {name} Current bounded peer transport resources.\n# TYPE {name} gauge\n{name} {value}\n"));
    }
    out.push_str("# HELP pstore_peer_connections_created_total Established peer connections by transport.\n# TYPE pstore_peer_connections_created_total counter\n");
    for (name, count) in [
        ("h2", peer_stats.as_ref().map_or(0, |s| s.h2)),
        ("h3", peer_stats.as_ref().map_or(0, |s| s.h3)),
    ] {
        out.push_str(&format!(
            "pstore_peer_connections_created_total{{transport=\"{name}\"}} {count}\n"
        ));
    }
    for (name, value) in [
        (
            "pstore_peer_http3_probes_total",
            peer_stats.as_ref().map_or(0, |s| s.probes),
        ),
        (
            "pstore_peer_transport_fallbacks_total",
            peer_stats.as_ref().map_or(0, |s| s.fallbacks),
        ),
    ] {
        out.push_str(&format!("# HELP {name} Peer transport availability attempts.\n# TYPE {name} counter\n{name} {value}\n"));
    }
    for (transport, phases) in ["h2", "h3"].iter().zip(&api.parts_completed) {
        for (phase, counter) in ["whole", "open", "scan"].iter().zip(phases) {
            out.push_str(&format!("pstore_peer_parts_completed_total{{transport=\"{transport}\",phase=\"{phase}\"}} {}\n", counter.load(std::sync::atomic::Ordering::Relaxed)));
        }
    }
    let held_now = {
        let mut held = api.holder();
        held.sweep(tokio::time::Instant::now());
        held.len()
    };
    out.push_str(&format!("# HELP pstore_peer_parts_held Opened peer parts awaiting scans.\n# TYPE pstore_peer_parts_held gauge\npstore_peer_parts_held {held_now}\n"));
    out.push_str(&format!(
        "# HELP pstore_peer_parts_sent Parts of this server's queries sent to peers (M54).\n\
         # TYPE pstore_peer_parts_sent counter\npstore_peer_parts_sent {sent}\n\
         # HELP pstore_peer_parts_failed Parts a peer did not answer, run here instead.\n\
         # TYPE pstore_peer_parts_failed counter\npstore_peer_parts_failed {failed}\n\
         # HELP pstore_peer_parts_served Parts of other servers' queries run here.\n\
         # TYPE pstore_peer_parts_served counter\npstore_peer_parts_served {}\n\
         # HELP pstore_peer_parts_expired Parts held between phases and dropped unscanned.\n\
         # TYPE pstore_peer_parts_expired counter\npstore_peer_parts_expired {}\n",
        api.parts_served.load(std::sync::atomic::Ordering::Relaxed),
        {
            // A scrape sweeps too (code review): a peer that gets no more phased traffic still
            // drops, and counts, what has been held past its time.
            let mut held = api.holder();
            held.sweep(tokio::time::Instant::now());
            held.expired
        }
    ));
    out.push_str("# HELP pstore_refusals_total Refusals by their stable code.\n");
    out.push_str("# TYPE pstore_refusals_total counter\n");
    // ⚠️ Every code the table can produce is emitted, at zero if it has not happened: a series
    // that appears only once something goes wrong is a series no alert can be written against.
    for code in REFUSAL_CODES {
        out.push_str(&format!(
            "pstore_refusals_total{{code=\"{code}\"}} {}\n",
            http.refusals.get(code).copied().unwrap_or(0)
        ));
    }
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        out,
    )
        .into_response()
}

/// Every stable refusal code the error table can produce.
///
/// ⚠️ A list, so `/metrics` can emit a zero for each: an alert on a series that does not exist
/// until the first failure is an alert that fires late, if at all.
const REFUSAL_CODES: &[&str] = &[
    "unauthenticated",
    "forbidden",
    "tenant_required",
    "bad_request",
    "unsupported_durability",
    "index_not_found",
    "schema_conflict",
    "schema_immutable",
    "time_travel_horizon",
    "version_conflict",
    "storage_unavailable",
    "not_folded",
    "bad_session",
    "internal",
];

/// The refusal code, carried out of `IntoResponse` so a layer can count it.
#[derive(Debug, Clone, Copy)]
struct RefusalCode(&'static str);

/// The request's session token (M11.1), or an empty one when it carries none.
fn session_of(headers: &HeaderMap, tenant: TenantId) -> Result<session::Token, ApiError> {
    let Some(v) = headers.get(SESSION_HEADER) else {
        return Ok(session::Token::empty(tenant.0));
    };
    let text = v
        .to_str()
        .map_err(|e| ApiError::bad_session(format!("x-pstore-session: {e}")))?;
    session::Token::decode(text, tenant.0)
        .map_err(|e| ApiError::bad_session(format!("x-pstore-session: {e}")))
}

/// Where the session token travels, both ways (M11.1).
const SESSION_HEADER: &str = "x-pstore-session";

/// A response carrying `token` in [`SESSION_HEADER`].
fn with_session(body: impl IntoResponse, token: &str) -> Response {
    let mut res = body.into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(token) {
        res.headers_mut().insert(SESSION_HEADER, v);
    }
    res
}

/// Who the caller says it is.
///
/// ⚠️ **Never defaulted.** `unwrap_or(TenantId(0))` is a cross-tenant data leak that passes
/// every single-tenant test in the suite, so the absence of a header is a refusal and the
/// type system does not offer a second option.
fn tenant_of(headers: &HeaderMap) -> Result<TenantId, ApiError> {
    let refuse = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "tenant_required",
            "X-Pstore-Tenant must be present and a non-negative integer",
        )
    };
    headers
        .get("x-pstore-tenant")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u128>().ok())
        .map(TenantId)
        .ok_or_else(refuse)
}

/// Why no attribute may be named `name`, if none may.
fn name_refusal(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("has an empty name");
    }
    // `$` names are the engine's (M9d: `$metric` carries a row's metric to the fold), as
    // turbopuffer reserves them for `$dist`.
    if name.starts_with('$') {
        return Some("is reserved: names beginning `$` are");
    }
    // `id` in a filter always means the document id (M9b); an attribute of that name would
    // give `["id", "Eq", x]` two meanings.
    (name == pstore_query::ID_ATTRIBUTE).then_some("is reserved: `id` is the document id")
}

/// The document fields a column of `upsert_columns` may name; every other column is an
/// attribute.
const DOCUMENT_COLUMNS: [&str; 4] = ["vector", "vectors", "text", SPARSE_FIELD];

/// A write's upserts as one list (M174): `documents`, then the rows of `upsert_columns`, each
/// read as a `documents` entry is, with every `null` attribute left out -- an upsert replaces
/// the row, so a null is an attribute the row does not hold. Run as soon as the body is
/// parsed, so every rule that reads `documents` reads both.
///
/// Cells are moved, not copied, as `patch_rows_of` moves them (M120).
fn upserts_of(req: &mut WriteRequest) -> Result<(), ApiError> {
    if let Some(cols) = req.upsert_columns.take() {
        let bad = |why: String| ApiError::bad_request(format!("upsert_columns: {why}"));
        let serde_json::Value::Object(mut cols) = cols else {
            return Err(bad("an object of columns".to_owned()));
        };
        let ids = match cols.remove("id") {
            Some(serde_json::Value::Array(ids)) => ids,
            _ => return Err(bad("an `id` column is required".to_owned())),
        };
        let mut columns = Vec::with_capacity(cols.len());
        // A column's name is an attribute's, checked in every row that holds it, null or not;
        // a column of no rows names nothing.
        for (name, col) in cols {
            match col {
                serde_json::Value::Array(c) if c.len() == ids.len() => {
                    columns.push((name, c.into_iter()));
                }
                _ => return Err(bad(format!("column `{name}` must have one value per id"))),
            }
        }
        for id in ids {
            // M176: a string or an integer, as a row's id is; the row reads it again.
            let shown = types::Id::of(&id).map_err(bad)?;
            let mut attrs = serde_json::Map::new();
            let mut row = serde_json::Map::new();
            // Each column's cells in id order, one a row. A null cell is left to the row's
            // reading: no vector, no fields, no text, and an attribute dropped below.
            for (name, v) in columns
                .iter_mut()
                .filter_map(|(name, col)| col.next().map(|v| (name.as_str(), v)))
            {
                let to = if DOCUMENT_COLUMNS.contains(&name) {
                    &mut row
                } else {
                    &mut attrs
                };
                to.insert(name.to_owned(), v);
            }
            row.insert("attributes".to_owned(), attrs.into());
            row.insert("id".to_owned(), id);
            let doc = serde_json::from_value(row.into())
                .map_err(|e| bad(format!("document {shown}: {e}")))?;
            req.documents.push(doc);
        }
    }
    for d in &mut req.documents {
        // A null's name is checked before it is dropped; every other name is `to_document`'s.
        if let Some((name, why)) = d
            .attributes
            .iter()
            .filter(|(_, v)| v.is_null())
            .find_map(|(n, _)| name_refusal(n).map(|why| (n, why)))
        {
            return Err(ApiError::bad_request(format!(
                "document {}: attribute `{name}` {why}",
                d.id
            )));
        }
        d.attributes.retain(|_, v| !v.is_null());
    }
    Ok(())
}

/// A write's patches (M13), from `patch_rows` and `patch_columns`, in that order. Values are
/// converted as a document's attributes are; `null` removes an attribute.
///
/// ⚠️ Takes both by value (M120): every attribute map and every column cell was copied out of
/// the request, which nothing reads again.
fn patch_rows_of(
    patch_rows: Vec<serde_json::Value>,
    patch_columns: Option<serde_json::Value>,
) -> Result<Vec<RawPatch>, ApiError> {
    let mut rows: Vec<RawPatch> = Vec::new();
    for mut raw in patch_rows {
        let o = raw
            .as_object_mut()
            .ok_or_else(|| ApiError::bad_request("a patch is {\"id\", \"attributes\"}"))?;
        let id = o
            .get("id")
            .ok_or_else(|| "a patch needs an id".to_owned())
            .and_then(types::Id::of)
            .map_err(ApiError::bad_request)?;
        if let Some(k) = o.keys().find(|k| *k != "id" && *k != "attributes") {
            return Err(ApiError::bad_request(
                if k == "vector" || k == SPARSE_FIELD {
                    format!("patch of {id}: a patch carries no {k}; upsert the row to change it")
                } else {
                    format!("patch of {id}: unknown field {k}")
                },
            ));
        }
        let attrs = match o.remove("attributes") {
            None => serde_json::Map::new(),
            Some(serde_json::Value::Object(m)) => m,
            Some(_) => {
                return Err(ApiError::bad_request(format!(
                    "patch of {id}: attributes is an object"
                )));
            }
        };
        rows.push((id, attrs));
    }
    if let Some(mut cols) = patch_columns {
        let bad = |why: &str| ApiError::bad_request(format!("patch_columns: {why}"));
        let cols = cols
            .as_object_mut()
            .ok_or_else(|| bad("an object of columns"))?;
        let ids = match cols.remove("id") {
            Some(serde_json::Value::Array(ids)) => ids,
            _ => return Err(bad("an `id` column is required")),
        };
        let mut columns = Vec::new();
        for (name, col) in std::mem::take(cols) {
            if name == "vector" || name == SPARSE_FIELD {
                return Err(bad(&format!(
                    "a patch carries no {name}; upsert the rows to change them"
                )));
            }
            let col = match col {
                serde_json::Value::Array(c) if c.len() == ids.len() => c,
                _ => return Err(bad("every column must have one value per id")),
            };
            columns.push((name, col.into_iter()));
        }
        for id in &ids {
            let id = types::Id::of(id).map_err(|e| bad(&e))?;
            // Each column's cells in id order, one a row, moved.
            let attrs = columns
                .iter_mut()
                .filter_map(|(name, col)| col.next().map(|v| (name.clone(), v)))
                .collect();
            rows.push((id, attrs));
        }
    }
    Ok(rows)
}

/// Every id a write names read by the index's id kind (M176), `Some(None)` a string and
/// `None` unknown: a string as a `uuid`'s form under `uuid`, refused when it spells none; an
/// id of another kind refused as a conflict; and ids of two kinds refused. Under no known
/// kind the engine judges the rows. Rewrites the ids only with `apply`, so a refusal by a
/// stale kind can be judged again by a fresh one.
fn typed_ids(
    docs: &mut [types::DocumentIn],
    patches: &mut [RawPatch],
    deletes: &mut [types::Id],
    kind: Option<Option<pstore_format::typed::Kind>>,
    apply: bool,
) -> Result<(), ApiError> {
    use pstore_format::typed::Kind;
    let name = |k: Option<Kind>| k.map_or("string", Kind::name);
    // An id of another kind than the index's is a conflict with its schema, as the engine
    // names one; a string spelling no uuid is malformed.
    let conflict = |id: &types::Id, want: Option<Kind>| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "schema_conflict",
            format!("id {id} is not a {0}; this index's ids are {0}", name(want)),
        )
    };
    let ids = docs
        .iter_mut()
        .map(|d| &mut d.id)
        .chain(patches.iter_mut().map(|(id, _)| id))
        .chain(deletes.iter_mut());
    let mut first = None;
    for id in ids {
        let held = pstore_format::id::kind(id.as_str());
        let k = match kind {
            Some(Some(Kind::Uuid)) if held.is_none() => {
                let u = pstore_format::typed::parse_uuid(id.as_str())
                    .ok_or_else(|| ApiError::bad_request(format!("id {id} is not a uuid")))?;
                if apply {
                    id.0 = pstore_format::id::uuid(&u);
                }
                Some(Kind::Uuid)
            }
            Some(want) if held != want => return Err(conflict(id, want)),
            _ => held,
        };
        match first {
            None => first = Some(k),
            Some(f) if f != k => {
                return Err(ApiError::bad_request(format!(
                    "id {id} is of another kind than the write's first id: an index's ids are \
                     all strings, all uint64 or all uuid"
                )));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// The id kind a write reads its ids by (M176), from `dated` (the known kinds and the
/// request's declarations) and whether the request declares `id: string`.
fn id_kind(
    dated: &typed::Declarations,
    declared_string: bool,
) -> Result<Option<Option<pstore_format::typed::Kind>>, ApiError> {
    match (dated.get(pstore_engine::ID_KIND), declared_string) {
        (Some(k), true) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "schema_conflict",
            format!("id is declared string; this index's ids are {}", k.name()),
        )),
        (Some(k), false) => Ok(Some(Some(*k))),
        (None, true) => Ok(Some(None)),
        (None, false) => Ok(None),
    }
}

/// A write's documents, patches and by-filter patch, their attributes read by `dated`.
type TypedOps = (
    Vec<Document>,
    Vec<pstore_engine::Patch>,
    Option<(pstore_query::Predicate, pstore_engine::Patch)>,
);

fn typed_ops(
    docs: Vec<types::DocumentIn>,
    patches: Vec<RawPatch>,
    patch_by: Option<(
        pstore_query::Predicate,
        serde_json::Map<String, serde_json::Value>,
    )>,
    dated: &typed::Declarations,
) -> Result<TypedOps, ApiError> {
    let patches = patches_of(patches, dated)?;
    let patch_by = patch_by
        .map(|(f, attrs)| patch_by_filter_attrs(attrs, dated).map(|p| (f, p)))
        .transpose()?;
    let docs = docs
        .into_iter()
        .map(|d| to_document(d, dated))
        .collect::<Result<_, _>>()?;
    Ok((docs, patches, patch_by))
}

/// A patch as a request spells it: its id and its attributes, not yet read by any type.
type RawPatch = (types::Id, serde_json::Map<String, serde_json::Value>);

/// The patches `rows` spell, their attributes read by `dated` (M173: the known types, so
/// after the HEAD read a refusal of the request's shape never pays for).
fn patches_of(
    rows: Vec<RawPatch>,
    dated: &typed::Declarations,
) -> Result<Vec<pstore_engine::Patch>, ApiError> {
    rows.into_iter()
        .map(|(id, attrs)| {
            let who = format!("patch of {id}");
            patch_of(id, &who, attrs, dated)
        })
        .collect()
}

/// One patch of `id`: `attributes` converted as a document's are, `null` removing one.
/// Refusals name `who`.
fn patch_of(
    id: types::Id,
    who: &str,
    attrs: serde_json::Map<String, serde_json::Value>,
    dated: &typed::Declarations,
) -> Result<pstore_engine::Patch, ApiError> {
    let unset: Vec<String> = attrs
        .iter()
        .filter(|(_, v)| v.is_null())
        .map(|(k, _)| k.clone())
        .collect();
    let set: serde_json::Map<String, serde_json::Value> =
        attrs.into_iter().filter(|(_, v)| !v.is_null()).collect();
    // Converted as a document's attributes are: every refusal of one is a patch's too.
    // ⚠️ Built, not parsed from a `json!` copy (M119): an empty vector and a map of values are
    // what that parse produced, and it could refuse neither, so it copied every value twice
    // to say nothing.
    let as_doc = types::DocumentIn {
        id: id.clone(),
        vector: None,
        vectors: std::collections::BTreeMap::new(),
        text: None,
        sparse: None,
        attributes: set.into_iter().collect(),
    };
    for name in &unset {
        if name.is_empty() || name.starts_with('$') || name == pstore_query::ID_ATTRIBUTE {
            return Err(ApiError::bad_request(format!(
                "{who}: attribute `{name}` is reserved"
            )));
        }
    }
    let doc = to_document(as_doc, dated)?;
    Ok(pstore_engine::Patch {
        id: id.0,
        set: doc.attrs,
        unset,
    })
}

/// The filter a write names under `name` (M13): parsed as a query's is, and refused when the
/// fold could not read it back -- one nested too deeply (code review, M13.1).
fn write_filter(name: &str, v: &serde_json::Value) -> Result<pstore_query::Predicate, ApiError> {
    let p = predicate(v).map_err(|e| ApiError::bad_request(format!("{name}: {}", e.message)))?;
    pstore_query::condition::decode(&pstore_query::condition::encode(&p))
        .map(|_| p)
        .ok_or_else(|| ApiError::bad_request(format!("{name}: the filter nests too deeply")))
}

/// `patch_by_filter` (M13.2): `{"filters": .., "attributes": {..}}`, both required.
fn patch_by_filter_of(
    v: &serde_json::Value,
) -> Result<
    (
        pstore_query::Predicate,
        serde_json::Map<String, serde_json::Value>,
    ),
    ApiError,
> {
    let bad = || {
        ApiError::bad_request(
            "patch_by_filter is {\"filters\": <filter>, \"attributes\": {..}}, both required",
        )
    };
    let o = v.as_object().ok_or_else(bad)?;
    if o.len() != 2 {
        return Err(bad());
    }
    let (Some(f), Some(serde_json::Value::Object(attrs))) = (o.get("filters"), o.get("attributes"))
    else {
        return Err(bad());
    };
    let filter = write_filter("patch_by_filter", f)?;
    Ok((filter, attrs.clone()))
}

/// `patch_by_filter`'s attributes, read by `dated` as [`patches_of`] reads a patch's.
fn patch_by_filter_attrs(
    attrs: serde_json::Map<String, serde_json::Value>,
    dated: &typed::Declarations,
) -> Result<pstore_engine::Patch, ApiError> {
    // Refusals from the document conversion name no operation, so they are prefixed here.
    patch_of(types::Id(String::new()), "patch_by_filter", attrs, dated).map_err(|e| {
        if e.message.starts_with("patch_by_filter") {
            e
        } else {
            ApiError::bad_request(format!("patch_by_filter: {}", e.message))
        }
    })
}

/// What a write's operations will hold in its tenant's buffer (M141), by
/// [`pstore_format::Document::footprint`]'s rules: documents, each patch's id and
/// attributes, each delete's tombstone, each by-filter operation's encoded filter, and an
/// applied request's `stamp` on every row. An estimate: the engine trues it up to the
/// stored rows as they are buffered.
fn write_footprint(
    docs: &[Document],
    patches: &[pstore_engine::Patch],
    deletes: &[String],
    delete_by: Option<&pstore_query::Predicate>,
    patch_by: Option<&(pstore_query::Predicate, pstore_engine::Patch)>,
    stamp: u64,
) -> u64 {
    let patch = |p: &pstore_engine::Patch| {
        p.id.len() as u64
            + p.set
                .iter()
                .map(|(k, v)| k.len() as u64 + v.footprint())
                .sum::<u64>()
            + p.unset.iter().map(|k| k.len() as u64).sum::<u64>()
    };
    let filter = |f: &pstore_query::Predicate| pstore_query::condition::encode(f).len() as u64;
    // A delete is stored as a tombstone: its id and an 8-byte mark under an empty name.
    let rows = docs.len()
        + patches.len()
        + deletes.len()
        + usize::from(delete_by.is_some())
        + usize::from(patch_by.is_some());
    docs.iter().map(Document::footprint).sum::<u64>()
        + patches.iter().map(patch).sum::<u64>()
        + deletes.iter().map(|d| d.len() as u64 + 8).sum::<u64>()
        + delete_by.map_or(0, filter)
        + patch_by.map_or(0, |(f, p)| filter(f) + patch(p))
        + rows as u64 * stamp
}

/// Reserves `bytes` of `tenant`'s buffer for one write (M141), flushing first when a
/// batched write would pass the flush bound, or refuses.
///
/// ⚠️ **The flush bound is soft**: one flush, then the reservation is retried without it,
/// so writes racing this one can pass it by their own size. The buffer bound is hard.
async fn reserve_buffer<'e, S: BlobStore + 'static>(
    api: &Api<S>,
    tenant: TenantId,
    engine: &'e Tenant<S>,
    bytes: u64,
    batched: bool,
) -> Result<pstore_engine::BufferReservation<'e>, ApiError> {
    let limits = api.ingress.limits();
    if bytes > limits.tenant_buffered_bytes() {
        return Err(ApiError {
            message: "the write's rows are larger than a tenant's buffer bound".to_owned(),
            ..ingress::too_large()
        });
    }
    let bounds = pstore_engine::BufferBounds {
        flush_at: batched.then_some(limits.tenant_flush_bytes()),
        cap: limits.tenant_buffered_bytes(),
    };
    let refused = match engine.reserve_buffer(bytes, bounds) {
        Ok(r) => return Ok(r),
        Err(pstore_engine::BufferRefusal::Flush) => {
            engine.flush().await?;
            match engine.reserve_buffer(
                bytes,
                pstore_engine::BufferBounds {
                    flush_at: None,
                    ..bounds
                },
            ) {
                Ok(r) => return Ok(r),
                Err(e) => e,
            }
        }
        Err(e) => e,
    };
    debug_assert_eq!(refused, pstore_engine::BufferRefusal::Full);
    api.requested
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(tenant);
    Err(ApiError {
        message: "this tenant's buffered rows are at their bound; a fold has been requested, \
                  retry once it commits (with scheduled folds off, POST /v1/admin/fold)"
            .to_owned(),
        ..ingress::overloaded()
    })
}

/// `PUT /v1/indexes/{index}/documents`.
async fn write_documents<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<auth::Principal>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let tenant = tenant_of(&headers)?;
    let mut token = session_of(&headers, tenant)?;
    // ⚠️ Parsed here rather than by the `Json` extractor, because the extractor's own
    // rejection is a body this API did not design -- and an unknown `durability` must come
    // back as `unsupported_durability` rather than as a serde message about an enum variant.
    let (mut req, keys) = parse_write(&body)?;
    upserts_of(&mut req)?;
    let applied = matches!(req.durability, types::Durability::Applied);
    let ids_wanted = req.affected_ids;
    if applied && !api.registry_mut().applied_writes {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "unsupported_durability",
            "durability \"applied\" is not enabled on this deployment (PSTORE_APPLIED_WRITES)",
        ));
    }
    // M130: ids are what a fold decided, so only an applied write can report them.
    if ids_wanted && !applied {
        return Err(ApiError::bad_request(
            "affected_ids needs durability \"applied\": only a fold knows which ids changed",
        ));
    }
    if ids_wanted && !api.registry_mut().affected_ids {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "unsupported_durability",
            "affected_ids is not enabled on this deployment (PSTORE_APPLIED_WRITES=2)",
        ));
    }
    let key = idempotency_key_of(&api, req.idempotency_key.as_deref(), applied)?;
    allow_partial(&api, &req, applied)?;
    if let Some(src) = branch_source(&req, keys)? {
        principal.source(tenant, "")?;
        return branch_index(&api, tenant, token, src, &index).await;
    }
    let types = declared_types(&req)?;
    let declared = pstore_engine::Declared {
        fts: declared_fts(&req)?,
        trigram: declared_trigram(&req)?,
        types: types.clone(),
        texts: declared_further(&req)?,
    };
    let patches = patch_rows_of(
        std::mem::take(&mut req.patch_rows),
        req.patch_columns.take(),
    )?;
    let (delete_partial, patch_partial) = (
        req.delete_by_filter_allow_partial,
        req.patch_by_filter_allow_partial,
    );
    let delete_by = req
        .delete_by_filter
        .as_ref()
        .map(|v| write_filter("delete_by_filter", v))
        .transpose()?;
    let patch_by = req
        .patch_by_filter
        .as_ref()
        .map(patch_by_filter_of)
        .transpose()?;
    if req.documents.is_empty()
        && req.deletes.is_empty()
        && patches.is_empty()
        && delete_by.is_none()
        && patch_by.is_none()
    {
        return Err(ApiError::bad_request(
            "a write with no documents, patches or deletes",
        ));
    }
    // M13: each condition guards its own kind of operation, and must have some to guard.
    // A condition travels to the fold encoded; one the fold could not decode would admit
    // nothing there, so it is refused here (code review, M13.1). M133: an upsert's or a
    // patch's may hold `$ref_new`, resolved per operation below.
    let condition = |name: &str, v: &Option<serde_json::Value>, any: bool| match v {
        Some(_) if !any => Err(ApiError::bad_request(format!(
            "{name} with no operations of its kind to apply to"
        ))),
        _ => Ok(v.clone()),
    };
    let upsert_if = condition(
        "upsert_condition",
        &req.upsert_condition,
        !req.documents.is_empty(),
    )?
    .map(|v| ref_new::write_cond("upsert_condition", &v))
    .transpose()?;
    let patch_if = condition("patch_condition", &req.patch_condition, !patches.is_empty())?
        .map(|v| ref_new::write_cond("patch_condition", &v))
        .transpose()?;
    let delete_if = condition(
        "delete_condition",
        &req.delete_condition,
        !req.deletes.is_empty(),
    )?
    .map(|v| write_filter("delete_condition", &v))
    .transpose()?;
    // M139: what an `affected_ids` write leaves each of its by-filter operations.
    let filter_rows = if ids_wanted {
        let by_filter = delete_by.is_some() || patch_by.is_some();
        let served = api.registry_mut().filter_ids;
        affected_ids_bounded(&req, &patches, by_filter, served)?
    } else {
        None
    };
    // ⚠️ **Deferred operations are durable only** (M13, spec review M4): a batched one would sit
    // where no fold reads it and no probe finds it, so a strong read here could miss it.
    let deferred = !patches.is_empty()
        || upsert_if.is_some()
        || delete_if.is_some()
        || delete_by.is_some()
        || patch_by.is_some();
    if deferred
        && !matches!(
            req.durability,
            types::Durability::Durable | types::Durability::Applied
        )
    {
        return Err(ApiError::bad_request(
            "patches, conditional writes and by-filter operations are durable only: they are \
             applied at a fold, which \
             reads only what a flush made durable",
        ));
    }
    // ⚠️ An unknown metric is refused, never read as the default (M9d): a client that asked
    // for cosine and got dot products would be told nothing.
    let metric = match req.distance_metric.as_deref() {
        None => pstore_engine::Metric::DotProduct,
        Some(name) => pstore_engine::Metric::parse(name).ok_or_else(|| {
            ApiError::bad_request(format!(
                "distance_metric {name:?} is not one of cosine_distance, euclidean_squared, \
                 dot_product"
            ))
        })?,
    };
    widths_agree(&req)?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    // M173: every attribute is read by its known type -- recorded, or declared by this
    // process's unfolded writes -- and by the request's own; the engine refuses a declaration
    // that differs from a known one.
    // Read only for a write holding a value to read: one with none, deletes or vectors alone,
    // is refused by M141's bound, or buffered, without a blob request.
    let valued = req.documents.iter().any(|d| !d.attributes.is_empty())
        || patches.iter().any(|(_, attrs)| !attrs.is_empty())
        || patch_by
            .as_ref()
            .is_some_and(|(_, attrs)| !attrs.is_empty());
    // M176: and every id by the index's id kind -- needed only to read a string spelling a
    // uuid as one, so a write naming none reads nothing more: any other id of the wrong kind
    // is the engine's to refuse, at zero requests, as M141's bound needs.
    let uuids = req
        .documents
        .iter()
        .map(|d| &d.id)
        .chain(patches.iter().map(|(id, _)| id))
        .chain(req.deletes.iter())
        .any(|id| pstore_format::typed::parse_uuid(id.as_str()).is_some());
    let mut dated = if valued || uuids {
        engine.known_types(&index).await
    } else {
        pstore_engine::Types::new()
    };
    dated.extend(types.iter().map(|(n, k)| (n.clone(), *k)));
    let mut patches = patches;
    let declared_string = req
        .schema
        .as_ref()
        .and_then(|s| s.get(pstore_query::ID_ATTRIBUTE))
        .is_some_and(|ty| ty == "string");
    // A refusal by a kind the request did not declare re-reads HEAD once before it stands,
    // as an attribute type's does (M173): the kind held may be a dropped index's.
    if let Err(e) = id_kind(&dated, declared_string)
        .and_then(|k| typed_ids(&mut req.documents, &mut patches, &mut req.deletes, k, false))
    {
        if !dated.contains_key(pstore_engine::ID_KIND) || types.contains_key(pstore_engine::ID_KIND)
        {
            return Err(e);
        }
        dated = engine.reread_types(&index).await?;
        dated.extend(types.iter().map(|(n, k)| (n.clone(), *k)));
    }
    typed_ids(
        &mut req.documents,
        &mut patches,
        &mut req.deletes,
        id_kind(&dated, declared_string)?,
        true,
    )?;
    // ⚠️ Every document converted BEFORE the engine sees any: a refusal must leave nothing
    // buffered, not the half of the batch that came before the bad attribute.
    // Moved, not copied (M119): the documents are not read again -- unless a type this
    // request did not declare reads them. A refusal by one re-reads HEAD once before it
    // stands (M9f.2), because the type held may be a dropped index's, and the read needs
    // the request as it came.
    // The id's kind reads no attribute (M176), so it alone asks for no copy.
    let kept = dated
        .keys()
        .any(|n| n != pstore_engine::ID_KIND && !types.contains_key(n))
        .then(|| (req.documents.clone(), patches.clone(), patch_by.clone()));
    let (docs, patches, patch_by) = match (
        typed_ops(
            std::mem::take(&mut req.documents),
            patches,
            patch_by,
            &dated,
        ),
        kept,
    ) {
        (Ok(ops), _) => ops,
        (Err(e), None) => return Err(e),
        (Err(e), Some((d, p, b))) => {
            let mut fresh = engine.reread_types(&index).await?;
            fresh.extend(types);
            if fresh == dated {
                return Err(e);
            }
            typed_ops(d, p, b, &fresh)?
        }
    };
    check_further(&docs, declared.texts.as_ref())?;
    check_pre_tokenized(&docs, declared.fts.as_ref(), declared.texts.as_ref())?;
    admit(&engine, key.as_ref()).await?;

    // M133: every reference resolved, and refused, before anything is buffered.
    let ops: Vec<_> = docs.iter().map(|d| (d.id.as_str(), &d.attrs)).collect();
    let upsert_if = ref_new::conds("upsert_condition", upsert_if, &ops)?;
    let ops: Vec<_> = patches.iter().map(|p| (p.id.as_str(), &p.set)).collect();
    let patch_if = ref_new::conds("patch_condition", patch_if, &ops)?;
    // M129: an applied write's window starts at the HEAD this engine knew BEFORE its
    // operations are buffered, and every one of them carries its id to the fold.
    let op = applied.then(operation_id).transpose()?;
    let mark = match op {
        Some(_) => Some(engine.mark().await?),
        None => None,
    };
    let deletes: Vec<String> = std::mem::take(&mut req.deletes)
        .into_iter()
        .map(|id| id.0)
        .collect();
    // M141: the whole request's share of the tenant's buffer, before anything is buffered.
    let need = write_footprint(
        &docs,
        &patches,
        &deletes,
        delete_by.as_ref(),
        patch_by.as_ref(),
        pstore_engine::stamp_footprint(op, ids_wanted, key.as_ref()),
    );
    let batched = matches!(req.durability, types::Durability::Batched);
    let reserved = reserve_buffer(&api, tenant, &engine, need, batched).await?;
    let buffer = async {
        let written = docs.len();
        if !docs.is_empty() {
            upsert(&engine, &index, docs, metric, upsert_if, &declared).await?;
        }
        // Then the patches, then the deletes (M13): a later operation on an id wins.
        let patched = patches.len();
        if !patches.is_empty() {
            patch_with(&engine, &index, patches, patch_if).await?;
        }
        // After the documents: a request that writes and deletes an id deletes it.
        let deleted = deletes.len();
        match &delete_if {
            Some(c) => engine.delete_if(&index, deletes, c).await?,
            None => engine.delete(&index, deletes).await?,
        }
        // Then the by-filter operations (M13.2), each one deferred operation: a patch, then a
        // delete, so a request that does both deletes what it patched into matching.
        // M137: either may allow partial application. M139: and with `affected_ids`, each is
        // bounded by what the write's named ids leave.
        let how = |partial| pstore_engine::ByFilter {
            partial,
            rows: filter_rows,
        };
        if let Some((filter, patch)) = patch_by {
            engine
                .patch_by_filter_bounded(&index, &filter, patch, how(patch_partial))
                .await?;
        }
        if let Some(filter) = &delete_by {
            engine
                .delete_by_filter_bounded(&index, filter, how(delete_partial))
                .await?;
        }
        Ok::<_, ApiError>((written, patched, deleted))
    };
    let (written, patched, deleted) = reserved
        .scope(pstore_engine::stamped(op, ids_wanted, key.as_ref(), buffer))
        .await?;
    let durable = matches!(
        req.durability,
        types::Durability::Durable | types::Durability::Applied
    );
    if durable {
        engine.flush().await?;
        // ⚠️ **The engine's next sequence, not this flush's own** (M11.1, spec review B1): a
        // concurrent request's flush may have written these rows already, and this one then
        // flushed nothing. Every bundle the engine wrote is below its next sequence.
        if let Some(next) = engine.next_seq() {
            token.wrote(engine.lane().0, next);
        }
    }
    let outcome = match (op, mark) {
        (Some(op), Some(mark)) => Some((op, engine.await_outcome(mark, op, key.as_ref()).await?)),
        _ => None,
    };
    let answer = applied_answer(outcome, ids_wanted)?;
    token.saw(engine.epoch().0);
    let session = token.encode();
    Ok(with_session(
        axum::Json(WriteResponse {
            epoch: engine.epoch().0,
            documents_written: written,
            documents_deleted: deleted,
            documents_patched: patched,
            durable,
            cost: api.spend(tenant).since(before),
            session: session.clone(),
            completion: answer.completion,
            operation: answer.operation,
            applied: answer.applied,
            outcome_unavailable: answer.outcome_unavailable,
            replayed: answer.replayed,
        }),
        &session,
    ))
}

/// What an applied write's response says of its outcome (M129, M132).
struct AppliedAnswer {
    completion: Option<&'static str>,
    operation: Option<String>,
    applied: Option<Option<types::AppliedOut>>,
    outcome_unavailable: Option<bool>,
    replayed: Option<bool>,
}

/// The response fields for an applied write's outcome, or its refusal (M132): an attempt the
/// fold refused applied nothing, or only part, and says so. ⚠️ Not generic over the store,
/// so one test's instantiation covers it.
fn applied_answer(
    outcome: Option<(pstore_engine::OperationId, pstore_engine::Applied)>,
    ids_wanted: bool,
) -> Result<AppliedAnswer, ApiError> {
    use pstore_engine::Applied;
    match outcome.as_ref().map(|(_, a)| a) {
        Some(Applied::Expired) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "idempotency_expired",
                "this idempotency key's retention has passed and nothing of this write was \
                 applied; send it again with a new key",
            ));
        }
        Some(Applied::Partial) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "idempotency_partial",
                "this write was, or may have been, applied, in whole or in part, and what \
                 would say which is no longer listed; read the rows before sending it again",
            ));
        }
        _ => {}
    }
    let replayed = outcome
        .as_ref()
        .is_some_and(|(_, a)| matches!(a, Applied::Replayed(_)))
        .then_some(true);
    let completion = outcome.as_ref().map(|_| "applied");
    let operation = outcome.as_ref().map(|(op, _)| op.hex());
    let outcome_unavailable = outcome
        .as_ref()
        .map(|(_, a)| matches!(a, Applied::Unavailable | Applied::Replayed(None)))
        .filter(|u| *u);
    let applied = outcome.map(|(_, a)| match a {
        Applied::Known(o, ids) | Applied::Replayed(Some((o, ids))) => Some(types::AppliedOut {
            upserted: o.upserted,
            deleted: o.deleted,
            patched: o.patched,
            skipped: o.skipped,
            rejected: o.rejected,
            rows_remaining: o.rows_remaining.then_some(true),
            ids: ids_wanted
                .then(|| ids.unwrap_or_default())
                .map(|i| types::AffectedIds {
                    upserted: i.upserted,
                    deleted: i.deleted,
                    patched: i.patched,
                }),
        }),
        Applied::Unavailable | Applied::Replayed(None) | Applied::Expired | Applied::Partial => {
            None
        }
    });
    Ok(AppliedAnswer {
        completion,
        operation,
        applied,
        outcome_unavailable,
        replayed,
    })
}

/// Refuses `allow_partial` (M137) beside any durability but `applied`, which alone can say
/// `rows_remaining`; without its by-filter operation; and below `PSTORE_APPLIED_WRITES=4`.
fn allow_partial<S: BlobStore>(
    api: &Api<S>,
    req: &types::WriteRequest,
    applied: bool,
) -> Result<(), ApiError> {
    let flags = [
        (
            "delete_by_filter_allow_partial",
            req.delete_by_filter_allow_partial,
            req.delete_by_filter.is_some(),
        ),
        (
            "patch_by_filter_allow_partial",
            req.patch_by_filter_allow_partial,
            req.patch_by_filter.is_some(),
        ),
    ];
    for (name, set, has_op) in flags {
        if !set {
            continue;
        }
        if !has_op {
            let op = name.trim_end_matches("_allow_partial");
            return Err(ApiError::bad_request(format!("{name} needs a {op}")));
        }
        if !applied {
            return Err(ApiError::bad_request(format!(
                "{name} needs durability \"applied\": only an applied answer can say rows remain"
            )));
        }
        if !api.registry_mut().partial_filters {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "unsupported_durability",
                format!("{name} is not enabled on this deployment (PSTORE_APPLIED_WRITES=4)"),
            ));
        }
    }
    Ok(())
}

/// The write's idempotency key, if it carries one (M132): refused beside any durability but
/// `applied`, below `PSTORE_APPLIED_WRITES=3`, or when it is not spelled as one is issued.
fn idempotency_key_of<S: BlobStore + 'static>(
    api: &Api<S>,
    key: Option<&str>,
    applied: bool,
) -> Result<Option<pstore_engine::IdempotencyKey>, ApiError> {
    let Some(key) = key else {
        return Ok(None);
    };
    if !applied {
        return Err(ApiError::bad_request(
            "idempotency_key needs durability \"applied\": only a fold decides which attempt \
             applies",
        ));
    }
    if !api.registry_mut().idempotency {
        return Err(keys_not_enabled());
    }
    pstore_engine::IdempotencyKey::parse(key)
        .map(Some)
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "idempotency_key {key:?} is not one this API issues: <epoch>-<32 lowercase hex>"
            ))
        })
}

/// Buffers `docs` under `conds` (M133: one each, when the condition held `$ref_new`).
async fn upsert<S: BlobStore + 'static>(
    engine: &Engine<S>,
    index: &str,
    docs: Vec<Document>,
    metric: pstore_engine::Metric,
    conds: ref_new::Conds,
    declared: &pstore_engine::Declared,
) -> Result<(), EngineError> {
    match conds {
        ref_new::Conds::One(c) => {
            engine
                .write_declared(index, docs, metric, c.as_ref(), declared)
                .await
        }
        ref_new::Conds::Each(cs) => {
            engine
                .write_conditioned(index, docs, cs, metric, declared)
                .await
        }
    }
}

/// Buffers `patches` under `conds`, as [`upsert`] does.
async fn patch_with<S: BlobStore + 'static>(
    engine: &Engine<S>,
    index: &str,
    patches: Vec<pstore_engine::Patch>,
    conds: ref_new::Conds,
) -> Result<(), EngineError> {
    match conds {
        ref_new::Conds::One(c) => engine.patch(index, patches, c.as_ref()).await,
        ref_new::Conds::Each(cs) => engine.patch_conditioned(index, patches, cs).await,
    }
}

/// M132: refuses a key from ahead of HEAD, which would let a commit at or below its `e0`
/// apply it.
async fn admit<S: BlobStore + 'static>(
    engine: &Engine<S>,
    key: Option<&pstore_engine::IdempotencyKey>,
) -> Result<(), ApiError> {
    match key {
        Some(k) if !engine.admit_key(k).await? => Err(ApiError::bad_request(format!(
            "idempotency_key {k} was not issued by this deployment: its epoch is ahead of HEAD"
        ))),
        _ => Ok(()),
    }
}

fn keys_not_enabled() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "unsupported_durability",
        "idempotency keys are not enabled on this deployment (PSTORE_APPLIED_WRITES=3)",
    )
}

/// `POST /v1/indexes/{index}/idempotency-keys` (M132): a fresh key, issued at this engine's
/// mark. Free while the mark is fresh; one HEAD read a second at most otherwise. Keys are
/// per tenant: the index only scopes the route.
async fn issue_idempotency_key<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(_index): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<types::IdempotencyKeyResponse>, ApiError> {
    let tenant = tenant_of(&headers)?;
    if !api.registry_mut().idempotency {
        return Err(keys_not_enabled());
    }
    let engine = api.engine(tenant).await;
    let random = operation_id()?.0;
    let key = engine.idempotency_key(random).await?;
    Ok(axum::Json(types::IdempotencyKeyResponse {
        idempotency_key: key.to_string(),
    }))
}

/// The most distinct ids an `affected_ids` write may name (M130): its lists are bounded by
/// what it names, so its response and its outcome object are too.
const AFFECTED_IDS_MAX: usize = 10_000;

/// Refuses an `affected_ids` write whose lists could be unbounded (M130): one naming more
/// than [`AFFECTED_IDS_MAX`] distinct ids, or one with a by-filter operation, whose ids are
/// whatever its filter admits at the fold, below `PSTORE_APPLIED_WRITES=5`.
///
/// M139: at 5, a by-filter operation's bound is what the named ids leave of
/// [`AFFECTED_IDS_MAX`], returned; a write naming all of them leaves none, and is refused.
fn affected_ids_bounded(
    req: &WriteRequest,
    patches: &[RawPatch],
    by_filter: bool,
    served: bool,
) -> Result<Option<usize>, ApiError> {
    if by_filter && !served {
        return Err(ApiError::bad_request(
            "affected_ids beside a by-filter operation is not served: its ids are unbounded",
        ));
    }
    let named: std::collections::HashSet<&str> = req
        .documents
        .iter()
        .map(|d| d.id.as_str())
        .chain(req.deletes.iter().map(types::Id::as_str))
        .chain(patches.iter().map(|(id, _)| id.as_str()))
        .collect();
    if named.len() > AFFECTED_IDS_MAX {
        return Err(ApiError::bad_request(format!(
            "affected_ids names {} distinct ids; at most {AFFECTED_IDS_MAX}",
            named.len()
        )));
    }
    if !by_filter {
        return Ok(None);
    }
    match AFFECTED_IDS_MAX - named.len() {
        0 => Err(ApiError::bad_request(format!(
            "affected_ids names {AFFECTED_IDS_MAX} distinct ids, leaving none for a by-filter \
             operation's; name fewer"
        ))),
        left => Ok(Some(left)),
    }
}

/// A fresh applied operation's id (M129): 128 random bits.
fn operation_id() -> Result<pstore_engine::OperationId, ApiError> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("no randomness for an operation id: {e}"),
        )
    })?;
    Ok(pstore_engine::OperationId(u128::from_le_bytes(b)))
}

/// The index a write asks to branch from (M16), if it asks -- refused unless the request says
/// nothing else, since a branch beside a batch, a schema or a durability would have to mean an
/// order or a promise the branch does not make.
fn branch_source(req: &WriteRequest, keys: usize) -> Result<Option<&str>, ApiError> {
    let src = match (&req.branch_from_namespace, &req.copy_from_namespace) {
        (None, None) => return Ok(None),
        (Some(s), None) | (None, Some(s)) => s.as_str(),
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request(
                "branch_from_namespace and copy_from_namespace are one operation, and must come \
                 alone: name one",
            ));
        }
    };
    // ⚠️ Counted on the raw body: `durability` has a default, so the parsed request cannot
    // tell a stated one from an absent one. The count is the parse's own (M122).
    if keys != 1 {
        return Err(ApiError::bad_request(
            "a branch must come alone: no documents, patches, deletes, conditions, schema, \
             distance_metric or durability beside it",
        ));
    }
    Ok(Some(src))
}

/// A write that branches `dest` from `src` (M16): the tenant folded first, so the branch holds
/// every acknowledged durable write, then one commit.
async fn branch_index<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    tenant: TenantId,
    mut token: session::Token,
    src: &str,
    dest: &str,
) -> Result<Response, ApiError> {
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    engine.fold().await?;
    let epoch = engine.branch(src, dest).await?;
    token.saw(epoch.0);
    let session = token.encode();
    Ok(with_session(
        axum::Json(WriteResponse {
            epoch: epoch.0,
            documents_written: 0,
            documents_deleted: 0,
            documents_patched: 0,
            durable: true,
            cost: api.spend(tenant).since(before),
            session: session.clone(),
            completion: None,
            operation: None,
            applied: None,
            outcome_unavailable: None,
            replayed: None,
        }),
        &session,
    ))
}

/// `POST /v1/indexes/{index}/query`.
async fn query_index<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let tenant = tenant_of(&headers)?;
    let token = session_of(&headers, tenant)?;
    let fast = fast_query(&body);
    let raw: serde_json::Value = if fast.is_some() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("malformed query: {e}")))?
    };
    // ⚠️ Every query is planned -- every refusal found -- before any runs (M9g): one refused
    // sub-query refuses the request, and nothing partial is returned.
    if fast.is_some() || raw.get("queries").is_none() {
        let req = match fast {
            Some(req) => checked_query(req)?,
            None => parse_query(raw)?,
        };
        // M65: checked first, so every refusal names `facets`.
        let facet = facets::derive(&req)?;
        let plan = plan(&req)?;
        let engine = api.engine(tenant).await;
        let before = api.spend(tenant);
        let peers = api.query_peers(tenant, &req);
        let (got, faceted) = match facet {
            None => {
                let ranked = run(&engine, &index, &req, plan, &token, peers.as_ref());
                (api.requesting(tenant, ranked.await).await?, None)
            }
            Some((freq, _)) => {
                // ⚠️ Together, never one after the other: depth is the deeper of the two
                // (M65). M131: and from one snapshot, unless the query reads a past epoch;
                // planned inside, so a pinned retry can run both again.
                let (req, freq, token, peers, engine, index) =
                    (&req, &freq, &token, peers.as_ref(), &engine, index.as_str());
                let both = move || async move {
                    let ranked = run(engine, index, req, crate::plan(req)?, token, peers);
                    let beside = run(engine, index, freq, crate::plan(freq)?, token, None);
                    futures_util::future::try_join(ranked, beside).await
                };
                let both = match pinnable(std::slice::from_ref(req), token)? {
                    Some(level) => engine.pinned(index, level, both).await,
                    None => both().await,
                };
                let (got, faceted) = api.requesting(tenant, both).await?;
                (got, Some(faceted))
            }
        };
        let facets_epoch = faceted.as_ref().map(|f| f.epoch);
        let facets = faceted.map(|f| facets::render(f.aggregations, f.aggregation_groups));
        let session = got.token.encode();
        return Ok(with_session(
            axum::Json(QueryResponse {
                results: got.results,
                aggregations: got.aggregations,
                aggregation_groups: got.aggregation_groups,
                facets,
                meta: QueryMeta {
                    facets_epoch,
                    epoch: got.epoch,
                    unfolded_hits: got.unfolded_hits,
                    cost: api.spend(tenant).since(before),
                    consistency: got.consistency,
                    session: session.clone(),
                    staleness_ms: got.staleness_ms,
                },
            }),
            &session,
        ));
    }
    let (reqs, outer) = multi_query(raw)?;
    // M164: every facet query derived, and every refusal found, before any runs -- facets
    // first, as for a query alone (M65), so every refusal names `facets`.
    let facet_reqs = reqs
        .iter()
        .map(|r| Ok(facets::derive(r)?.map(|(f, _)| f)))
        .collect::<Result<Vec<_>, ApiError>>()?;
    reqs.iter().map(plan).collect::<Result<Vec<_>, _>>()?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    // Concurrently: a multi-query costs its queries' requests at the deepest one's depth.
    let peers: Vec<Option<peers::QueryPeers>> =
        reqs.iter().map(|r| api.query_peers(tenant, r)).collect();
    // M131: every leg from one snapshot when they share a level and none reads a past epoch.
    let whole = pinnable(&reqs, &token)?;
    // Planned again inside, so a pinned retry (M131) can run every leg once more.
    let all = {
        let (reqs, facet_reqs, peers, engine, index, token) =
            (&reqs, &facet_reqs, &peers, &engine, &index, &token);
        let pinned_whole = whole.is_some();
        move || {
            futures_util::future::try_join_all(reqs.iter().zip(facet_reqs).zip(peers).map(
                move |((req, facet), p)| async move {
                    // A query and its facets, together: depth is the deeper of the two (M65).
                    let pair = move || async move {
                        let ranked = run(engine, index, req, plan(req)?, token, p.as_ref());
                        match facet {
                            None => Ok::<_, ApiError>((ranked.await?, None)),
                            Some(f) => {
                                let beside = run(engine, index, f, plan(f)?, token, None);
                                let (got, faceted) =
                                    futures_util::future::try_join(ranked, beside).await?;
                                Ok((got, Some(faceted)))
                            }
                        }
                    };
                    // M164: a request not pinned as a whole still pins each query to its
                    // facets, as `query_index` pins a faceted query alone.
                    let own = match facet {
                        Some(_) if !pinned_whole => pinnable(std::slice::from_ref(req), token)?,
                        _ => None,
                    };
                    match own {
                        Some(level) => engine.pinned(index, level, pair).await,
                        None => pair().await,
                    }
                },
            ))
        }
    };
    let got = match whole {
        Some(level) => engine.pinned(&index, level, all).await,
        None => all().await,
    };
    let got = api.requesting(tenant, got).await?;
    // From the sub-queries' own tokens, never the request's: each already carries it, less
    // the entries its HEAD covers, and merging the request's back would restore them.
    let mut merged = got
        .first()
        .map_or_else(|| token.clone(), |(g, _)| g.token.clone());
    for (g, _) in got.iter().skip(1) {
        merged.merge(&g.token);
    }
    let session = merged.encode();
    // M164: the aggregation fields when any query aggregates, the facets when any has them.
    let aggregating = reqs.iter().any(|r| r.aggregate_by.is_some());
    let faceting = facet_reqs.iter().any(Option::is_some);
    let mut aggregations = Vec::with_capacity(got.len());
    let mut aggregation_groups = Vec::with_capacity(got.len());
    let mut facets = Vec::with_capacity(got.len());
    let mut facets_epochs = Vec::with_capacity(got.len());
    let mut epochs = Vec::with_capacity(got.len());
    let mut unfolded_hits = Vec::with_capacity(got.len());
    let mut consistencies = Vec::with_capacity(got.len());
    let mut staleness_ms = Vec::with_capacity(got.len());
    let mut lists = Vec::with_capacity(got.len());
    for (g, faceted) in got {
        epochs.push(g.epoch);
        unfolded_hits.push(g.unfolded_hits);
        consistencies.push(g.consistency);
        staleness_ms.push(g.staleness_ms);
        aggregations.push(g.aggregations);
        aggregation_groups.push(g.aggregation_groups);
        facets_epochs.push(faceted.as_ref().map(|f| f.epoch));
        facets.push(faceted.map(|f| facets::render(f.aggregations, f.aggregation_groups)));
        lists.push(g.results);
    }
    Ok(with_session(
        axum::Json(MultiQueryResponse {
            meta: MultiQueryMeta {
                epochs,
                unfolded_hits,
                cost: api.spend(tenant).since(before),
                consistencies,
                session: session.clone(),
                staleness_ms,
                facets_epochs: faceting.then_some(facets_epochs),
            },
            aggregations: aggregating.then_some(aggregations),
            aggregation_groups: aggregating.then_some(aggregation_groups),
            facets: faceting.then_some(facets),
            results: match &outer {
                // M142: one list, the fused one.
                Some(outer) => vec![rerank::fuse_outer(lists, outer)],
                None => lists,
            },
        }),
        &session,
    ))
}

/// One query, parsed once into its type where that is the tree's answer (M120, [`body`]), or
/// `None` for the tree to parse. A body naming `queries` is a multi-query, the tree's, and is
/// told apart before any typed parse: its sub-queries are not parsed twice to be discarded.
fn fast_query(raw: &[u8]) -> Option<QueryRequest> {
    body::plain(raw)?
        .keys
        .filter(|k| !k.iter().any(|k| k == "queries"))?;
    serde_json::from_slice(raw).ok()
}

/// The most queries one request may carry (M9g), turbopuffer's bound.
const MAX_QUERIES: usize = 16;

/// A query body, or why it is not one.
fn parse_query(raw: serde_json::Value) -> Result<QueryRequest, ApiError> {
    let req: QueryRequest = serde_json::from_value(raw)
        .map_err(|e| ApiError::bad_request(format!("malformed query: {e}")))?;
    checked_query(req)
}

/// A parsed query, or why it is not one.
fn checked_query(req: QueryRequest) -> Result<QueryRequest, ApiError> {
    if req.top_k == 0 {
        return Err(ApiError::bad_request("top_k must be greater than zero"));
    }
    // M143: as an aggregation's or a rank_by order's, which keep their own messages.
    if req.top_k > MAX_ORDERED && req.rank_by.is_none() && req.aggregate_by.is_none() {
        return Err(ApiError::bad_request(format!(
            "top_k may be at most {MAX_ORDERED} for a query leg"
        )));
    }
    Ok(req)
}

/// The root keys a multi-query may carry besides `queries` (M142): its outer fusion.
const OUTER_KEYS: [&str; 3] = ["rerank_by", "limit", "offset"];

/// `{"queries": [...]}`'s sub-queries (M9g): 1 to 16, none nested -- and its outer fusion,
/// when the root names `rerank_by` (M142). Every other root key is refused.
///
/// ⚠️ Takes the body by value (M119): each sub-query was deep-copied, vector and all, only to
/// hand `parse_query` one it could own.
fn multi_query(
    raw: serde_json::Value,
) -> Result<(Vec<QueryRequest>, Option<rerank::Outer>), ApiError> {
    if raw.as_object().is_some_and(|o| {
        o.keys()
            .any(|k| k != "queries" && !OUTER_KEYS.contains(&k.as_str()))
    }) {
        return Err(ApiError::bad_request(
            "queries is the whole request, beside rerank_by, limit and offset: each query \
             carries its own fields",
        ));
    }
    let (subs, outer) = match raw {
        serde_json::Value::Object(mut o) => match o.remove("queries") {
            Some(serde_json::Value::Array(a)) if (1..=MAX_QUERIES).contains(&a.len()) => {
                let outer = match o.get("rerank_by") {
                    Some(r) => Some(
                        rerank::Outer::parse(r, o.get("limit"), o.get("offset"), a.len())
                            .map_err(ApiError::bad_request)?,
                    ),
                    None if o.is_empty() => None,
                    None => {
                        return Err(ApiError::bad_request(
                            "limit and offset apply only beside rerank_by",
                        ));
                    }
                };
                Some((a, outer))
            }
            _ => None,
        },
        _ => None,
    }
    .ok_or_else(|| {
        ApiError::bad_request(format!("queries is an array of 1 to {MAX_QUERIES} queries"))
    })?;
    let subs = subs
        .into_iter()
        .map(|q| {
            if q.get("queries").is_some() {
                return Err(ApiError::bad_request("a query inside queries may not nest"));
            }
            // M179: a multi-query's rows are fused and merged; their vectors are later.
            if q.get("include_vectors").is_some() {
                return Err(ApiError::bad_request(
                    "include_vectors is a single query's: a query inside queries cannot \
                     project vectors yet",
                ));
            }
            parse_query(q)
        })
        .collect::<Result<Vec<QueryRequest>, _>>()?;
    // M164: fusion ranks rows, and an aggregation has none.
    if outer.is_some() && subs.iter().any(|q| q.aggregate_by.is_some()) {
        return Err(ApiError::bad_request(
            "rerank_by fuses rankings: a query inside it cannot be an aggregate_by",
        ));
    }
    Ok((subs, outer))
}

/// An aggregation's labels, groups and bound (M12), or `None` for a query that asks for rows.
fn aggregation(req: &QueryRequest) -> Result<Option<pstore_query::AggregateSpec>, ApiError> {
    use pstore_query::Aggregate;
    let Some(raw) = &req.aggregate_by else {
        if req.group_by.is_some() {
            return Err(ApiError::bad_request("group_by without aggregate_by"));
        }
        return Ok(None);
    };
    for (field, present) in [
        ("vector", req.vector.is_some()),
        ("multi", req.multi.is_some()),
        ("text", req.text.is_some()),
        ("sparse", req.sparse.is_some()),
        ("rank_by", req.rank_by.is_some()),
        ("offset", req.offset.is_some()),
        ("fusion", req.fusion.is_some()),
        ("include_attributes", req.include_attributes.is_some()),
        ("exclude_attributes", req.exclude_attributes.is_some()),
        ("include_vectors", req.include_vectors.is_some()),
    ] {
        if present {
            return Err(ApiError::bad_request(format!(
                "aggregate_by cannot be combined with {field}"
            )));
        }
    }
    let labels = raw
        .as_object()
        .filter(|o| (1..=MAX_AGGREGATES).contains(&o.len()))
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "aggregate_by maps 1 to {MAX_AGGREGATES} labels to an aggregate"
            ))
        })?;
    let mut out = Vec::with_capacity(labels.len());
    for (label, v) in labels {
        let malformed =
            || ApiError::bad_request(format!("aggregate {label} must be [kind, attribute]"));
        let (kind, attr) = match v.as_array().map(Vec::as_slice) {
            Some([kind, attr]) => (
                kind.as_str().ok_or_else(malformed)?,
                attr.as_str().ok_or_else(malformed)?,
            ),
            _ => return Err(malformed()),
        };
        let agg = match (kind, attr) {
            ("Count", pstore_query::ID_ATTRIBUTE) => Aggregate::Count(None),
            ("Count", a) => Aggregate::Count(Some(a.to_owned())),
            ("Sum", pstore_query::ID_ATTRIBUTE) => {
                return Err(ApiError::bad_request(format!(
                    "aggregate {label}: Sum of id is not a number"
                )));
            }
            ("Sum", a) => Aggregate::Sum(a.to_owned()),
            (other, _) => {
                return Err(ApiError::bad_request(format!(
                    "aggregate {label}: {other} is not Count or Sum"
                )));
            }
        };
        out.push((label.clone(), agg));
    }
    let group_by: Vec<String> = match &req.group_by {
        None => Vec::new(),
        Some(v) => v
            .as_array()
            .filter(|a| (1..=MAX_GROUP_BY).contains(&a.len()))
            .and_then(|a| {
                a.iter()
                    .map(|n| n.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| {
                ApiError::bad_request(format!("group_by is 1 to {MAX_GROUP_BY} attribute names"))
            })?,
    };
    if let Some((label, _)) = out.iter().find(|(l, _)| group_by.contains(l)) {
        return Err(ApiError::bad_request(format!(
            "label {label} is also a group_by attribute: a group would carry it twice"
        )));
    }
    // As an order's: the bound on what one query holds per segment (code review).
    if req.top_k > MAX_ORDERED {
        return Err(ApiError::bad_request(format!(
            "top_k may be at most {MAX_ORDERED} groups for an aggregation"
        )));
    }
    Ok(Some(pstore_query::AggregateSpec {
        labels: out,
        group_by,
        top_k: req.top_k,
    }))
}

/// Labels one aggregation may compute (M12).
const MAX_AGGREGATES: usize = 16;
/// Attributes one aggregation may group by (M12).
const MAX_GROUP_BY: usize = 8;

/// A total as JSON: a float sum that overflowed has no JSON number, so it is `null` (M12).
fn total_json(t: pstore_query::Total) -> serde_json::Value {
    match t {
        pstore_query::Total::Int(n) => serde_json::Value::from(n),
        pstore_query::Total::Uint(n) => serde_json::Value::from(n),
        pstore_query::Total::Float(f) => serde_json::Value::from(f),
        pstore_query::Total::Null => serde_json::Value::Null,
    }
}

/// What a query will run, decided -- and refused -- before it runs.
enum Plan {
    /// Counts and sums, optionally grouped (M12).
    Aggregate(pstore_query::AggregateSpec, Option<pstore_query::Predicate>),
    /// A `rank_by` order (M9e).
    Ordered(pstore_query::Order, Option<pstore_query::Predicate>),
    /// A relevance ranking: its legs, filter and fusion, and its boost (M62).
    Ranked(
        Vec<pstore_query::Prefetch>,
        Option<pstore_query::Predicate>,
        pstore_query::Fusion,
        Option<boost::Boost>,
        Option<highlight::Highlight>,
        Option<diversify::Diversify>,
        // Boxed: the plan's largest variant, by far, without it.
        Option<Box<limit_per::LimitPer>>,
    ),
}

/// The most rows one query projects vectors for (M179): a page of 1,000 vectors of width
/// 1,536 is about 6 MB of f32s, where `MAX_ORDERED` alone would allow ten times it.
const MAX_VECTOR_ROWS: usize = 1_000;

/// How the query's results spell their stored `vector` (M179), `None` when it asks for none,
/// or why it cannot ask.
fn vector_projection(req: &QueryRequest) -> Result<Option<VectorEncoding>, ApiError> {
    let bad = |why: &str| ApiError::bad_request(format!("include_vectors: {why}"));
    let Some(raw) = &req.include_vectors else {
        if req.vector_encoding.is_some() {
            return Err(ApiError::bad_request(
                "vector_encoding spells projected vectors: the query has no include_vectors",
            ));
        }
        return Ok(None);
    };
    let names = raw
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| bad("a list of the vector fields to project, [\"vector\"]"))?;
    if let Some(other) = names
        .iter()
        .find(|n| n.as_str() != Some(pstore_format::DEFAULT_FIELD))
    {
        return Err(bad(&format!(
            "{other} is not projected: only `vector` is, a named field's vectors are later"
        )));
    }
    let encoding = match req.vector_encoding.as_deref() {
        None | Some("float") => VectorEncoding::Float,
        Some("base64") => VectorEncoding::Base64,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "vector_encoding: {other:?} is not float or base64"
            )));
        }
    };
    if req.as_of.is_some() {
        return Err(bad(
            "a past epoch's vectors are later: it cannot be combined with as_of",
        ));
    }
    // A ranked query's row fetch covers its whole page; an order reads only the page's rows.
    let rows = if req.rank_by.is_some() {
        req.top_k
    } else {
        page_end(req)
    };
    if rows > MAX_VECTOR_ROWS {
        return Err(bad(&format!(
            "{rows} rows: vectors are projected for at most {MAX_VECTOR_ROWS} (top_k, and a \
             ranked query's offset)"
        )));
    }
    Ok(Some(encoding))
}

/// How a projected vector is spelled (M179).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VectorEncoding {
    Float,
    Base64,
}

impl VectorEncoding {
    /// `v` as this encoding spells it, or nothing for a row with no vector. The engine
    /// answers none, never an empty one, for a row of a width-0 index.
    fn spell(self, v: Option<&[f32]>) -> Option<types::VectorOut> {
        use base64::Engine as _;
        let v = v?;
        Some(match self {
            Self::Float => types::VectorOut::Float(v.to_vec()),
            Self::Base64 => {
                let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
                types::VectorOut::Base64(base64::engine::general_purpose::STANDARD.encode(bytes))
            }
        })
    }
}

/// A document's `vector`, as an order's rows carry it (M179).
fn stored_vector(d: &pstore_format::Document) -> Option<&[f32]> {
    match d.vectors.get(pstore_format::DEFAULT_FIELD)?.as_dense() {
        [v] => Some(v),
        _ => None,
    }
}

/// Where a relevance query's page ends: `top_k` past its `offset` (M163).
fn page_end(req: &QueryRequest) -> usize {
    req.top_k.saturating_add(req.offset.unwrap_or(0))
}

fn plan(req: &QueryRequest) -> Result<Plan, ApiError> {
    level(req)?;
    vector_projection(req)?;
    // M63: checked first, before an aggregation or an order returns, so every refusal names it.
    let highlight = req
        .highlight
        .as_ref()
        .map(highlight::parse)
        .transpose()
        .map_err(|why| ApiError::bad_request(format!("highlight: {why}")))?;
    if highlight.is_some() {
        let with = if req.rank_by.is_some() {
            Some("rank_by")
        } else if req.aggregate_by.is_some() {
            Some("aggregate_by")
        } else {
            None
        };
        if let Some(what) = with {
            return Err(ApiError::bad_request(format!(
                "highlight marks a text leg's terms: it cannot be combined with {what}"
            )));
        }
        if req.text.is_none() {
            return Err(ApiError::bad_request(
                "highlight marks a text leg's terms: the query has none",
            ));
        }
        // M155: M63 marks the primary's terms in one attribute; marking several fields is a
        // later milestone's structured highlights.
        let legs = req.text.as_ref().map(TextIn::legs).transpose();
        let legs = legs.map_err(ApiError::bad_request)?.unwrap_or_default();
        if let Some(f) = legs
            .iter()
            .filter_map(|(f, _, _)| *f)
            .find(|f| *f != pstore_format::text::DEFAULT_TEXT_FIELD)
        {
            return Err(ApiError::bad_request(format!(
                "highlight marks the text field's terms: it cannot mark the further text \
                 field `{f}`"
            )));
        }
    }
    // M64: checked at the top too, so every refusal names it. One window governs a boost and
    // a diversify: either names it, and both naming different ones is refused.
    let named_boost = req.boost.as_ref().and_then(diversify::named_window);
    let named_diversify = req.diversify.as_ref().and_then(diversify::named_window);
    if let (Some(b), Some(d)) = (named_boost, named_diversify)
        && b != d
    {
        return Err(ApiError::bad_request(format!(
            "diversify: window {d} differs from the boost's {b}; one window governs both"
        )));
    }
    let diversify = req
        .diversify
        .as_ref()
        .map(|d| diversify::parse(d, req.top_k, named_boost))
        .transpose()
        .map_err(|why| ApiError::bad_request(format!("diversify: {why}")))?;
    if diversify.is_some() {
        let with = if req.rank_by.is_some() {
            Some("rank_by")
        } else if req.aggregate_by.is_some() {
            Some("aggregate_by")
        } else if req.offset.is_some() {
            Some("offset")
        } else if req.as_of.is_some() {
            Some("as_of")
        } else {
            None
        };
        if let Some(what) = with {
            return Err(ApiError::bad_request(format!(
                "diversify re-orders a relevance ranking: it cannot be combined with {what}"
            )));
        }
    }
    // M146: a group limit cuts a relevance ranking; MMR would reorder what it cut.
    if req.limit_per.is_some() {
        let with = if req.rank_by.is_some() {
            Some("rank_by")
        } else if req.aggregate_by.is_some() {
            Some("aggregate_by")
        } else if req.diversify.is_some() {
            Some("diversify")
        } else {
            None
        };
        if let Some(what) = with {
            return Err(ApiError::bad_request(format!(
                "limit_per cuts a relevance ranking: it cannot be combined with {what}"
            )));
        }
    }
    // M62: a boost rescores a relevance ranking; an order or an aggregation has none.
    if req.boost.is_some() && (req.rank_by.is_some() || req.aggregate_by.is_some()) {
        return Err(ApiError::bad_request(
            "boost rescores a relevance ranking: it cannot be combined with rank_by or \
             aggregate_by",
        ));
    }
    let filter = req.filters.as_ref().map(predicate).transpose()?;
    // Before both: an aggregation has no vector, text or order by definition (M12).
    if let Some(spec) = aggregation(req)? {
        return Ok(Plan::Aggregate(spec, filter));
    }
    // ⚠️ Before `prefetch`, which refuses a query with no vector and no text: an ordered query
    // has neither by definition (M9e).
    if let Some(by) = order_by(req)? {
        return Ok(Plan::Ordered(by, filter));
    }
    // M146: a group limit names the window too (it never sits beside a diversify).
    // M163: a page's end is the depth every path ranks to; `offset` bounds it as an order's.
    let page_end = page_end(req);
    if req.offset.is_some() && page_end > MAX_ORDERED {
        return Err(ApiError::bad_request(format!(
            "top_k + offset may be at most {MAX_ORDERED} for a relevance ranking"
        )));
    }
    let named_limit = req.limit_per.as_ref().and_then(diversify::named_window);
    let boost = req
        .boost
        .as_ref()
        .map(|b| boost::parse(b, page_end, named_diversify.or(named_limit)))
        .transpose()
        .map_err(|why| ApiError::bad_request(format!("boost: {why}")))?;
    // ⚠️ Every leg cut at the window, not `top_k` (M62 spec review): a candidate a leg ranks
    // below `top_k` would otherwise never reach the fusion the boost rescores.
    // M146: one window for a boost and a group limit, as for a boost and a diversify.
    let limit_per = req
        .limit_per
        .as_ref()
        .map(|p| limit_per::LimitPer::parse(p, page_end, named_boost))
        .transpose()
        .map_err(|why| ApiError::bad_request(format!("limit_per: {why}")))?;
    let window = boost
        .as_ref()
        .map(|b| b.window)
        .or(diversify.map(|d| d.window))
        .or(limit_per.as_ref().map(|p| p.window));
    let legs = prefetch(req, window.unwrap_or(page_end))?;
    let fusion = fusion(req, legs.len())?;
    Ok(Plan::Ranked(
        legs,
        filter,
        fusion,
        boost,
        highlight,
        diversify,
        limit_per.map(Box::new),
    ))
}

/// What a query asked to reflect (M9i.2, M11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    /// The HEAD it reads and this process's own writes.
    Eventual,
    /// Every durable write any process acknowledged before its HEAD read, or a refusal.
    Strong,
    /// Every durable write its session token names, from a HEAD no older than the token's.
    Session,
    /// A HEAD no older than this that this process already read, and its own writes (M11.2).
    Bounded(std::time::Duration),
}

/// The loosest bound a `bounded` read may ask for: D-39's fold age. Past it, one process
/// could serve a single HEAD indefinitely (M11.2, spec review).
const MAX_STALENESS_MS: u64 = 3_600_000;

/// A query's consistency: `eventual` unless it asks for `strong` or `session`, and refused
/// for anything else -- a level it cannot promise is never served as one it can.
fn level(req: &QueryRequest) -> Result<Level, ApiError> {
    let level = match &req.consistency {
        None | Some(serde_json::Value::Null) => Level::Eventual,
        Some(v) => match v.as_str() {
            Some("eventual") => Level::Eventual,
            Some("strong") => Level::Strong,
            Some("session") => Level::Session,
            Some("bounded") => Level::Bounded(staleness(req)?),
            _ => {
                return Err(ApiError::bad_request(format!(
                    "consistency {v} is not eventual, strong, session or bounded"
                )));
            }
        },
    };
    let bound = req.max_staleness_ms.as_ref().is_some_and(|v| !v.is_null());
    if bound && !matches!(level, Level::Bounded(_)) {
        return Err(ApiError::bad_request(
            "max_staleness_ms without consistency bounded",
        ));
    }
    if level != Level::Eventual && req.as_of.is_some() {
        return Err(ApiError::bad_request(
            "strong, session or bounded with as_of: a past epoch is already exactly what it is",
        ));
    }
    Ok(level)
}

/// A `bounded` read's tolerance (M11.2): an integer number of milliseconds up to
/// [`MAX_STALENESS_MS`], and required.
fn staleness(req: &QueryRequest) -> Result<std::time::Duration, ApiError> {
    req.max_staleness_ms
        .as_ref()
        .and_then(serde_json::Value::as_u64)
        .filter(|ms| *ms <= MAX_STALENESS_MS)
        .map(std::time::Duration::from_millis)
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "consistency bounded needs max_staleness_ms, an integer from 0 to \
                 {MAX_STALENESS_MS}"
            ))
        })
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Eventual => "eventual",
        Level::Strong => "strong",
        Level::Session => "session",
        Level::Bounded(_) => "bounded",
    }
}

/// What the engine is asked to do for `level`: a session whose token overflowed names no
/// lane, so it must cover every acknowledged write -- which is `strong` (M11.1).
fn engine_level(level: Level, token: &session::Token) -> pstore_engine::Consistency {
    match level {
        Level::Strong => pstore_engine::Consistency::Strong,
        Level::Session if token.overflow => pstore_engine::Consistency::Strong,
        Level::Eventual | Level::Session => pstore_engine::Consistency::Eventual,
        Level::Bounded(max) => pstore_engine::Consistency::Bounded(max),
    }
}

/// The level a request's legs share, when they can run from one snapshot (M131): every leg
/// at one engine level -- `session` mapped, a `bounded` tolerance part of it -- and none
/// `as_of` an epoch. `None` runs each leg on its own, as before.
fn pinnable(
    reqs: &[QueryRequest],
    token: &session::Token,
) -> Result<Option<pstore_engine::Consistency>, ApiError> {
    let mut shared = None;
    for req in reqs {
        if req.as_of.is_some() {
            return Ok(None);
        }
        let level = engine_level(level(req)?, token);
        if shared.is_some_and(|s| s != level) {
            return Ok(None);
        }
        shared = Some(level);
    }
    Ok(shared)
}

/// Checks a served answer against the session token and returns the token to send back
/// (M11.1). Reads nothing: `watermarks` are the served HEAD's, and the rest is memory.
///
/// ⚠️ An uncovered entry is `not_folded`, so the handler requests a fold exactly as for a
/// refused `strong` read (M9i.2).
fn after_read<E: BlobStore>(
    engine: &Engine<E>,
    level: Level,
    token: &session::Token,
    served: u64,
    watermarks: &std::collections::BTreeMap<u64, u64>,
) -> Result<session::Token, ApiError> {
    if level == Level::Session {
        if token.epoch > served {
            return Err(ApiError::bad_session(format!(
                "the token has seen epoch {}, and this store is at {served}: it did not come \
                 from here",
                token.epoch
            )));
        }
        if token
            .entries
            .iter()
            .any(|(lane, next)| !engine.covers(watermarks, LaneId(*lane), *next))
        {
            return Err(EngineError::NotFolded.into());
        }
    }
    let mut out = token.clone();
    out.saw(served);
    // Folded into what was served, so into every later HEAD: watermarks only grow.
    out.entries
        .retain(|lane, next| watermarks.get(lane).copied().unwrap_or(0) < *next);
    if engine_level(level, token) == pstore_engine::Consistency::Strong && out.overflow {
        // A strong read found nothing unfolded but this engine's own bundles, which only this
        // process holds: the one lane the cleared token must still name.
        out.overflow = false;
        if let Some(next) = engine.unfolded_next(watermarks) {
            out.wrote(engine.lane().0, next);
        }
    }
    Ok(out)
}

/// One query's answer, before it becomes a response.
struct Answered {
    consistency: &'static str,
    results: Vec<ResultRow>,
    epoch: u64,
    unfolded_hits: usize,
    /// The session token to send back (M11.1).
    token: session::Token,
    /// How old the HEAD served was (M11.2).
    staleness_ms: u64,
    /// An ungrouped aggregation's totals (M12).
    aggregations: Option<serde_json::Map<String, serde_json::Value>>,
    /// A grouped aggregation's groups (M12).
    aggregation_groups: Option<Vec<serde_json::Map<String, serde_json::Value>>>,
}

/// Runs a planned query.
///
/// ⚠️ **One read of HEAD per query.** Existence is decided from the snapshot the query already
/// read -- asking first would double the cost of every query, and resolving afterwards against a
/// second read would race a fold. `Answer::segments` is what carries it. Time travel is the
/// same query against a manifest reconstructed from the HEAD it already reads, and `epoch`
/// reports which epoch was served.
async fn run<E: BlobStore>(
    engine: &Engine<E>,
    index: &str,
    req: &QueryRequest,
    plan: Plan,
    token: &session::Token,
    peers: Option<&peers::QueryPeers>,
) -> Result<Answered, ApiError> {
    let missing = || {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "index_not_found",
            format!("this tenant has no index {index}"),
        )
    };
    let level = level(req)?;
    let consistency = engine_level(level, token);
    // A past epoch is what it is: the token passes through untouched.
    let checked = |served: u64, watermarks: &std::collections::BTreeMap<u64, u64>| {
        if req.as_of.is_some() {
            Ok(token.clone())
        } else {
            after_read(engine, level, token, served, watermarks)
        }
    };
    let projected = vector_projection(req)?;
    let (legs, filter, fusion, boost, highlight, diversify, limit_per) = match plan {
        Plan::Aggregate(spec, filter) => {
            let names: Vec<String> = spec.labels.iter().map(|(l, _)| l.clone()).collect();
            let group_by = spec.group_by.clone();
            let got = engine
                .aggregate_as(
                    index,
                    spec,
                    filter.as_ref(),
                    req.as_of.map(pstore_types::Epoch),
                    consistency,
                )
                .await?;
            // ⚠️ Before "no such index", as for an order.
            let token = checked(got.epoch.0, &got.watermarks)?;
            if !got.exists {
                return Err(missing());
            }
            let rows: Vec<serde_json::Map<String, serde_json::Value>> = got
                .groups
                .into_iter()
                .map(|(key, totals)| {
                    let mut row = serde_json::Map::new();
                    for (attr, k) in group_by.iter().zip(&key) {
                        let v = k.value().as_ref().map_or(serde_json::Value::Null, to_json);
                        row.insert(attr.clone(), v);
                    }
                    for (name, t) in names.iter().zip(totals) {
                        row.insert(name.clone(), total_json(t));
                    }
                    row
                })
                .collect();
            let (aggregations, aggregation_groups) = if group_by.is_empty() {
                (rows.into_iter().next(), None)
            } else {
                (None, Some(rows))
            };
            return Ok(Answered {
                token,
                staleness_ms: millis(got.staleness),
                results: Vec::new(),
                epoch: got.epoch.0,
                unfolded_hits: got.unfolded,
                consistency: level_name(level),
                aggregations,
                aggregation_groups,
            });
        }
        Plan::Ordered(by, filter) => {
            // M179: the page's vectors, one round after selection, only when asked.
            let got = if projected.is_some() {
                engine
                    .ordered_vectors(
                        index,
                        by,
                        filter.as_ref(),
                        req.offset.unwrap_or(0),
                        req.top_k,
                        consistency,
                    )
                    .await?
            } else {
                engine
                    .ordered_as(
                        index,
                        by,
                        filter.as_ref(),
                        req.offset.unwrap_or(0),
                        req.top_k,
                        req.as_of.map(pstore_types::Epoch),
                        consistency,
                    )
                    .await?
            };
            // ⚠️ Before "no such index": the session's unfolded write may be what creates it.
            let token = checked(got.epoch.0, &got.watermarks)?;
            if !got.exists {
                return Err(missing());
            }
            return Ok(Answered {
                token,
                staleness_ms: millis(got.staleness),
                aggregations: None,
                aggregation_groups: None,
                results: got
                    .rows
                    .into_iter()
                    .map(|d| ResultRow {
                        vector: projected.and_then(|e| e.spell(stored_vector(&d))),
                        attributes: selected(req, &d.attrs),
                        id: d.id,
                        score: None,
                        dist: None,
                        highlight: None,
                        highlights: None,
                    })
                    .collect(),
                epoch: got.epoch.0,
                unfolded_hits: got.unfolded,
                consistency: level_name(level),
            });
        }
        Plan::Ranked(legs, filter, fusion, boost, highlight, diversify, limit_per) => {
            (legs, filter, fusion, boost, highlight, diversify, limit_per)
        }
    };
    // M62: the engine ranks the window, and the boost cuts it to `top_k`.
    let top_k = boost
        .as_ref()
        .map(|b| b.window)
        .or(diversify.map(|d| d.window))
        .or(limit_per.as_ref().map(|p| p.window))
        .unwrap_or(page_end(req));
    let answer = match req.as_of {
        Some(e) => {
            engine
                .query_as_of_filtered(
                    index,
                    pstore_types::Epoch(e),
                    &legs,
                    filter.as_ref(),
                    fusion,
                    top_k,
                )
                .await?
        }
        // M64: the candidates' vectors ride the row fetch. (`as_of` is refused with it.) M179:
        // and a projection's, in the same fetch.
        None if diversify.is_some() || projected.is_some() => {
            engine
                .query_split_vectors(
                    index,
                    &legs,
                    filter.as_ref(),
                    fusion,
                    top_k,
                    consistency,
                    peers.map(|p| p as &dyn pstore_engine::Peers),
                )
                .await?
        }
        None => {
            engine
                .query_split_as(
                    index,
                    &legs,
                    filter.as_ref(),
                    fusion,
                    top_k,
                    consistency,
                    peers.map(|p| p as &dyn pstore_engine::Peers),
                )
                .await?
        }
    };
    let token = checked(answer.epoch.0, &answer.watermarks)?;
    // M147: a HEAD that names the index with no segment is an empty index, not a missing one.
    if !answer.named && answer.unfolded.is_empty() {
        return Err(missing());
    }
    // ⚠️ `Answer` is explicit that a hit at `unfolded_at` indexes the unfolded rows rather
    // than HEAD's segments -- M5f's finding, where keying on the row alone merged two
    // unrelated documents into one.
    let mut paired = engine.resolve_rows_with_vectors(&answer);
    if let Some(b) = &boost {
        for ((_, score, attrs, _), _) in &mut paired {
            *score = b.score(*score, attrs);
        }
        // Stable, so equal finals keep the engine's order.
        paired.sort_by(|x, y| y.0.1.total_cmp(&x.0.1));
    }
    if let Some(d) = diversify {
        // M64: the pick is `top_k × window × dims` multiply-adds, off the runtime's workers.
        let scores: Vec<f64> = paired.iter().map(|r| f64::from(r.0.1)).collect();
        // M179: copied, not taken, when the picked rows answer theirs.
        let vectors: Vec<Option<Vec<f32>>> = if projected.is_some() {
            paired.iter().map(|r| r.1.clone()).collect()
        } else {
            paired.iter_mut().map(|r| r.1.take()).collect()
        };
        let k = req.top_k;
        let order =
            tokio::task::spawn_blocking(move || diversify::select(&scores, &vectors, d.lambda, k))
                .await
                .map_err(|e| {
                    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
                })?;
        let mut slots: Vec<Option<_>> = paired.into_iter().map(Some).collect();
        paired = order
            .into_iter()
            .filter_map(|i| slots.get_mut(i).and_then(Option::take))
            .collect();
    }
    // M146: after the boost, on the stored attributes, whatever the query selects.
    if let Some(p) = &limit_per {
        paired = limit_per::per_group(paired, |((_, _, attrs, _), _)| attrs, p, page_end(req));
    }
    // M163: the page is cut at its end, then its first `offset` rows skipped.
    paired.truncate(page_end(req));
    paired.drain(..req.offset.unwrap_or(0).min(paired.len()));
    let rows = paired;
    let unfolded_hits =
        if boost.is_none() && diversify.is_none() && limit_per.is_none() && req.offset.is_none() {
            answer
                .hits
                .iter()
                .filter(|h| h.segment == answer.unfolded_at)
                .count()
        } else {
            // The rows were reordered and cut: a hit on the unfolded rows is one whose id an
            // unfolded row carries. Ids are unique in one answer, an id's newest version being the
            // only one served.
            let fresh: std::collections::HashSet<&str> = answer
                .hits
                .iter()
                .filter(|h| h.segment == answer.unfolded_at)
                .filter_map(|h| answer.unfolded.get(h.row).map(|d| d.id.as_str()))
                .collect();
            rows.iter()
                .filter(|((id, ..), _)| fresh.contains(id.as_str()))
                .count()
        };
    // M63: the query's terms, under the analyzer the ranking used; marked on the final rows
    // only, from the attributes the row round returned, whatever `include_attributes` asks.
    // M158: a pre-tokenized field's tokens are not words in a string, so there is nothing to
    // mark -- refused, as the vendor's does.
    if highlight.is_some()
        && answer.fts.analyzer.tokenizer == pstore_format::text::Tokenizer::PreTokenizedArray
    {
        return Err(ApiError::bad_request(
            "highlight: the text field is pre_tokenized_array, which has nothing to mark",
        ));
    }
    let marks = highlight.as_ref().map(|h| {
        let terms: std::collections::HashSet<String> = req
            .text
            .as_ref()
            .and_then(|t| t.legs().ok())
            .unwrap_or_default()
            .into_iter()
            // M180: the whole query's terms, a prefix leg's last token's own included.
            .flat_map(|(_, q, _)| pstore_format::text::analyze_query(&answer.fts.analyzer, &q))
            .collect();
        (h, terms)
    });
    Ok(Answered {
        results: rows
            .into_iter()
            .map(|((id, score, attrs, dist), vector)| ResultRow {
                vector: projected.and_then(|e| e.spell(vector.as_deref())),
                highlight: marks
                    .as_ref()
                    .filter(|(h, _)| h.structure.is_none())
                    .and_then(|(h, terms)| match attrs.get(&h.attr) {
                        Some(pstore_format::Value::Str(text)) => {
                            Some(h.render(text, &answer.fts.analyzer, terms))
                        }
                        _ => None,
                    }),
                highlights: marks.as_ref().and_then(|(h, terms)| {
                    h.fragments(attrs.get(&h.attr)?, &answer.fts.analyzer, terms)
                        .map(|f| f.into_iter().map(types::HighlightMatch::from).collect())
                }),
                attributes: selected(req, &attrs),
                id,
                score: Some(score),
                dist,
            })
            .collect(),
        epoch: answer.epoch.0,
        unfolded_hits,
        consistency: level_name(level),
        token,
        staleness_ms: millis(answer.staleness),
        aggregations: None,
        aggregation_groups: None,
    })
}

/// Milliseconds, rounded **up** and saturating: a staleness is never understated (M11.2).
fn millis(d: std::time::Duration) -> u64 {
    let partial = u128::from(!d.subsec_nanos().is_multiple_of(1_000_000));
    u64::try_from(d.as_millis() + partial).unwrap_or(u64::MAX)
}

/// How a query's legs combine (M9g.1), or why the request cannot mean it.
fn fusion(req: &QueryRequest, legs: usize) -> Result<pstore_query::Fusion, ApiError> {
    use pstore_query::{Fusion, Weights};
    let Some(raw) = &req.fusion else {
        return Ok(Fusion::Rrf { k: 60.0 });
    };
    let bad = |why: &str| ApiError::bad_request(format!("fusion: {why}"));
    let (kind, params) = raw
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.iter().next())
        .ok_or_else(|| bad("an object with one of rrf, sum or max"))?;
    let params = params
        .as_object()
        .ok_or_else(|| bad("its parameters are an object"))?;
    // A mistyped parameter is refused, never read as its default (code review).
    if let Some(other) = params
        .keys()
        .find(|p| !matches!(p.as_str(), "k" | "weights"))
    {
        return Err(bad(&format!(
            "{other:?} is not a parameter; k and weights are"
        )));
    }
    let weights = match params.get("weights") {
        None => None,
        Some(w) => {
            let ws = w
                .as_array()
                .ok_or_else(|| bad("weights is an array"))?
                .iter()
                .map(|x| {
                    x.as_f64()
                        .map(|f| f as f32)
                        .filter(|f| f.is_finite() && *f >= 0.0)
                        .ok_or_else(|| bad("each weight is a finite number of at least 0"))
                })
                .collect::<Result<Vec<f32>, _>>()?;
            if ws.len() != legs {
                return Err(bad(&format!(
                    "{} weights for {legs} legs; one per leg: dense, then multi, then text",
                    ws.len()
                )));
            }
            Some(Weights::of(&ws).ok_or_else(|| bad("more weights than legs allowed"))?)
        }
    };
    match kind.as_str() {
        "rrf" => {
            let k = match params.get("k") {
                None => 60.0,
                Some(k) => k
                    .as_f64()
                    .map(|f| f as f32)
                    .filter(|f| f.is_finite() && *f > 0.0)
                    .ok_or_else(|| bad("k is a finite number above 0"))?,
            };
            Ok(match weights {
                None => Fusion::Rrf { k },
                Some(weights) => Fusion::WeightedRrf { k, weights },
            })
        }
        // Text legs only (spec review): a dense score can be negative, so a row the dense leg
        // retrieved would rank below one it never found, whose missing leg adds nothing.
        "sum" | "max" if req.vector.is_some() || req.multi.is_some() => Err(bad(&format!(
            "{kind} combines text legs' scores; a dense or multi leg's can be negative"
        ))),
        // M178: a sparse dot product can be negative too, so it is summed with nothing; alone,
        // its score is the leg's own.
        "sum" | "max" if req.sparse.is_some() && req.text.is_some() => Err(bad(&format!(
            "{kind} combines text legs' scores; a sparse leg's can be negative, so it takes \
             {kind} only alone"
        ))),
        // `k` is RRF's alone, and a parameter is never accepted to be ignored (code review).
        "sum" | "max" if params.contains_key("k") => {
            Err(bad(&format!("k is rrf's parameter, not {kind}'s")))
        }
        "sum" => Ok(Fusion::Sum {
            weights: weights.unwrap_or(Weights::ONE),
        }),
        // Every weight zero ties every row at 0, and the tie then breaks over the union of the
        // legs' cuts rather than over what matched (spec review).
        "max" if weights.is_some_and(|w| w.all_zero(legs)) => {
            Err(bad("max with every weight 0 ranks nothing"))
        }
        "max" => Ok(Fusion::Max {
            weights: weights.unwrap_or(Weights::ONE),
        }),
        other => Err(bad(&format!("{other:?} is not rrf, sum or max"))),
    }
}

/// `GET /v1/indexes/{index}`.
async fn index_summary<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<axum::Json<IndexSummary>, ApiError> {
    let tenant = tenant_of(&headers)?;
    // M160: opt-in, so the summary without it keeps its one read.
    let progress = match params.get("progress").map(String::as_str) {
        None | Some("false") => false,
        Some("true") => true,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "progress must be true or false, not {other:?}"
            )));
        }
    };
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    // Beside the stats' HEAD read, not after it: no round added.
    let ((served, stats), progress) =
        futures_util::future::try_join(engine.index_stats_at(&index), async {
            match progress {
                true => engine.progress(&index).await.map(Some),
                false => Ok(None),
            }
        })
        .await?;
    let unfolded = engine.pending_indexes().await.iter().any(|i| i == &index);
    // A name only unfolded bundles hold is durable, not yet indexed (M160).
    let durable = progress.as_ref().is_some_and(|p| p.rows > 0);
    if stats.is_none() && !unfolded && !durable {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "index_not_found",
            format!("this tenant has no index {index}"),
        ));
    }
    let s = stats.unwrap_or(pstore_engine::IndexStats {
        segments: 0,
        documents: 0,
        // The HEAD read, never this process's last commit (M10.2, BACKLOG row 42).
        epoch: served,
        schema: None,
        rejected_rows: 0,
        quarantined_rows: 0,
        updated_epoch: None,
        created_ms: None,
        written_ms: None,
        bytes: Some(0),
        read_only: false,
    });
    Ok(axum::Json(IndexSummary {
        index,
        segments: s.segments,
        documents: s.documents,
        epoch: s.epoch.0,
        unfolded,
        schema: s.schema.map(|sc| SchemaOut {
            dims: sc.client_dims(),
            distance_metric: sc.metric.name(),
            text_field: sc.text_field,
            full_text_search: fts_json(&sc.fts),
            text_fields: sc
                .texts
                .iter()
                .map(|(n, f)| (n.clone(), fts_json(f)))
                .collect(),
            regex: sc.trigram,
            vectors: sc
                .fields
                .into_iter()
                .map(|(n, w)| (n, w.saturating_sub(sc.metric.extra() as u32)))
                .collect(),
            // M176: the id's kind is recorded among the types, under a name no attribute has.
            id: sc
                .types
                .get(pstore_engine::ID_KIND)
                .map_or("string", |k| k.name()),
            attributes: sc
                .types
                .into_iter()
                .filter(|(n, _)| n != pstore_engine::ID_KIND)
                .map(|(n, k)| (n, k.name()))
                .collect(),
        }),
        rejected_rows: s.rejected_rows,
        quarantined_rows: s.quarantined_rows,
        updated_epoch: s.updated_epoch.map(|e| e.0),
        approx_row_count: s.documents,
        created_at: s.created_ms.map(rfc3339_ms),
        updated_at: s.written_ms.map(rfc3339_ms),
        approx_logical_bytes: s.bytes,
        read_only: s.read_only,
        progress: progress.map(|p| IndexProgress {
            status: if p.rows > 0 { "updating" } else { "up-to-date" },
            unindexed_rows: p.rows,
            unindexed_bytes: p.bytes,
            exact: p.exact,
            as_of_epoch: p.epoch.0,
        }),
        cost: api.spend(tenant).since(before),
    }))
}

/// Unix milliseconds as RFC 3339 UTC, always with three fractional digits (M148).
fn rfc3339_ms(ms: u64) -> String {
    // `u64::MAX / 1000` fits an `i64`, so the fallback never runs; a far future saturates in
    // `format`'s microseconds.
    let secs = i64::try_from(ms / 1000).unwrap_or(i64::MAX);
    let whole = pstore_format::datetime::format(secs.saturating_mul(1_000_000));
    format!("{}.{:03}Z", whole.trim_end_matches('Z'), ms % 1000)
}

fn no_index(index: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "index_not_found",
        format!("this tenant has no index {index}"),
    )
}

/// `GET /v1/indexes/{index}/quarantine` (M25): every row a fold set aside, as a client would
/// write it again, with the `$` attributes that may say why, and the epoch a discard names.
async fn export_quarantine<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    let q = match engine.quarantine(&index).await {
        Ok(Some(q)) => q,
        Ok(None) => return Err(no_index(&index)),
        // ⚠️ An object HEAD names and the store lacks: never a shorter export, which a
        // client would take for the whole quarantine and discard.
        Err(pstore_engine::EngineError::Blob(e)) => {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "quarantine_unavailable",
                e,
            ));
        }
        Err(e) => return Err(e.into()),
    };
    let rows: Vec<serde_json::Value> = q
        .rows
        .iter()
        .map(|r| {
            let attrs: std::collections::BTreeMap<String, serde_json::Value> = r
                .document
                .attrs
                .iter()
                .map(|(k, v)| (k.clone(), to_json(v)))
                .collect();
            let reserved: std::collections::BTreeMap<String, serde_json::Value> = r
                .reserved
                .iter()
                .map(|(k, v)| (k.clone(), to_json(v)))
                .collect();
            serde_json::json!({
                "document": {
                    // M176: a typed id answered as written.
                    "id": to_json(&pstore_format::id::value(&r.document.id)),
                    "vector": r.document.vector(),
                    "attributes": attrs,
                },
                "reserved": reserved,
                "reason": r.reason,
            })
        })
        .collect();
    Ok(axum::Json(serde_json::json!({
        "epoch": q.epoch.0,
        "rows": rows,
        "cost": api.spend(tenant).since(before),
    })))
}

/// `DELETE /v1/indexes/{index}/quarantine?through={epoch}` (M25): buries what an export at
/// `through` showed, and nothing a later fold set aside.
async fn discard_quarantine<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let through = params
        .get("through")
        .and_then(|t| t.parse::<u64>().ok())
        .ok_or_else(|| {
            ApiError::bad_request(
                "through is required: the epoch the quarantine's export reported".to_owned(),
            )
        })?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    let n = engine
        .discard_quarantine(&index, pstore_types::Epoch(through))
        .await?
        .ok_or_else(|| no_index(&index))?;
    Ok(axum::Json(serde_json::json!({
        "discarded": n,
        "cost": api.spend(tenant).since(before),
    })))
}

/// `GET /v1/indexes`.
async fn list_indexes<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Result<axum::Json<IndexList>, ApiError> {
    let tenant = tenant_of(&headers)?;
    // 1 to 1000, 100 when absent (M9f): refused past either edge, never clamped.
    let page_size = match params.page_size.as_deref() {
        None => 100,
        Some(raw) => raw
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=1000).contains(n))
            .ok_or_else(|| {
                ApiError::bad_request(format!("page_size {raw:?} is not an integer 1 to 1000"))
            })?,
    };
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    // ⚠️ **One read, never a LIST.** A LIST over the tenant's prefix is the obvious
    // implementation, is correct, and is priced like a PUT while returning at most 1000 keys.
    let (epoch, mut names) = engine.indexes_at().await?;
    names.extend(engine.pending_indexes().await);
    names.sort_unstable();
    names.dedup();
    // Byte order, strictly after the cursor: a name is on exactly one page of a stable list.
    let prefix = params.prefix.unwrap_or_default();
    let cursor = params.cursor.unwrap_or_default();
    let mut page: Vec<String> = names
        .into_iter()
        .filter(|n| n.starts_with(&prefix) && n.as_str() > cursor.as_str())
        .take(page_size + 1)
        .collect();
    let next_cursor = if page.len() > page_size {
        page.truncate(page_size);
        page.last().cloned()
    } else {
        None
    };
    Ok(axum::Json(IndexList {
        indexes: page,
        next_cursor,
        epoch: epoch.0,
        cost: api.spend(tenant).since(before),
    }))
}

/// `POST /v1/admin/gc?retention=N` — reaps, and reports where history now ends.
///
/// ⚠️ **Added by M7e, and the spec is amended rather than the route smuggled in.** Without it
/// nothing in the server could ever reap: the graveyard would grow without bound and
/// `reaped_before` would stay zero forever, so the horizon `as_of` is bounded by would be a
/// bound no deployment could ever reach. A time-travel milestone that cannot move the horizon
/// has not built the thing it describes.
async fn gc_tenant<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<axum::Json<GcResponse>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let retention = params
        .get("retention")
        .map_or(Ok(DEFAULT_RETENTION), |r| r.parse::<u64>())
        .map_err(|e| ApiError::bad_request(format!("retention must be a number: {e}")))?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    let reaped = engine.gc(retention).await?;
    Ok(axum::Json(GcResponse {
        epoch: engine.epoch().0,
        reaped,
        reaped_before: engine.reap_horizon().await?,
        cost: api.spend(tenant).since(before),
    }))
}

/// `PATCH /v1/indexes/{index}/schema` — **refused, with the path named.**
///
/// ⚠️ A route that exists and refuses, rather than a `404`: the answer to "may I change this"
/// is *no, and here is what to do instead*, which is a different statement from "there is no
/// such thing". `api-design.md` says a schema change needing a reindex must say so.
async fn patch_schema(Path(index): Path<String>, headers: HeaderMap) -> ApiError {
    // The tenant is still required, so a caller cannot learn anything by omitting it.
    if let Err(e) = tenant_of(&headers) {
        return e;
    }
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "schema_immutable",
        format!(
            "index {index}'s schema is inferred by its first fold and cannot be changed in \
             place: segments already written cannot be reinterpreted. Write the corrected \
             documents to a new index and swap the name."
        ),
    )
}

/// `POST /v1/admin/fold` — folds **the header's tenant**, under the same rule as every route.
///
/// ⚠️ This is what makes a flushed write visible to a *different* process: until a fold, the
/// rows are durable in a lane bundle and named by nothing in HEAD.
async fn fold_tenant<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    headers: HeaderMap,
) -> Result<axum::Json<FoldResponse>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    let epoch = engine.fold().await?;
    Ok(axum::Json(FoldResponse {
        epoch: epoch.0,
        cost: api.spend(tenant).since(before),
    }))
}

/// `POST /v1/indexes/{index}/warm` (M21): `index`'s metadata into this process's read cache,
/// never its bulk (D-44). Billed. `404` for an index that does not exist, and `409`, before
/// any request, on a server without a cache: there a warm would fetch and discard.
async fn warm_index<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<WarmResponse>, ApiError> {
    let tenant = tenant_of(&headers)?;
    if api.cache.is_none() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "no_read_cache",
            "this server has no read cache to warm; start it with PSTORE_CACHE_DIR".to_owned(),
        ));
    }
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    let warmed = engine.warm(&index).await?;
    if !warmed.exists {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "index_not_found",
            format!("this tenant has no index {index}"),
        ));
    }
    Ok(axum::Json(WarmResponse {
        segments: warmed.segments,
        fetched: warmed.fetched,
        meta: WarmMeta {
            cost: api.spend(tenant).since(before),
        },
    }))
}

/// `DELETE /v1/indexes/{index}` (M9f.2): a fold that drops it. `404` if there is no such index,
/// and then nothing is committed.
async fn delete_index<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<FoldResponse>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    let Some(epoch) = engine.delete_index(&index).await? else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "index_not_found",
            format!("this tenant has no index {index}"),
        ));
    };
    Ok(axum::Json(FoldResponse {
        epoch: epoch.0,
        cost: api.spend(tenant).since(before),
    }))
}

/// `PUT /v1/indexes/{index}` (M147): creates the index with no rows, its width and metric
/// declared. One HEAD read and one HEAD CAS.
async fn create_index<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::Json<FoldResponse>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let req: CreateRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("malformed create request: {e}")))?;
    let most = api.ingress.limits().array_elements();
    let dims = u32::try_from(req.dims)
        .ok()
        .filter(|d| *d >= 1 && (*d as usize) <= most)
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "dims must be from 1 to {most}, the longest array this server admits, not {}",
                req.dims
            ))
        })?;
    let metric = match req.distance_metric.as_deref() {
        None => pstore_engine::Metric::DotProduct,
        Some(name) => pstore_engine::Metric::parse(name).ok_or_else(|| {
            ApiError::bad_request(format!(
                "distance_metric {name:?} is not one of cosine_distance, euclidean_squared, \
                 dot_product"
            ))
        })?,
    };
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    let epoch = engine.create_index(&index, dims, metric).await?;
    Ok(axum::Json(FoldResponse {
        epoch: epoch.0,
        cost: api.spend(tenant).since(before),
    }))
}

/// `PATCH /v1/indexes/{index}` (M149): makes the index read-only, or writable again.
async fn patch_index<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::Json<ReadOnlyResponse>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let req: PatchIndexRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("malformed index patch: {e}")))?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    let Some(epoch) = engine.set_read_only(&index, req.read_only).await? else {
        // HEAD does not hold it. One this process holds unfolded, which its summary
        // reports, is not absent: it can be flagged once folded.
        if engine.pending_indexes().await.iter().any(|i| i == &index) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "index_not_folded",
                format!("the index {index} has rows not yet folded; retry after the fold"),
            ));
        }
        return Err(no_index(&index));
    };
    Ok(axum::Json(ReadOnlyResponse {
        epoch: epoch.0,
        read_only: req.read_only,
        cost: api.spend(tenant).since(before),
    }))
}

/// What `PATCH /v1/indexes/{index}` carries (M149). Anything else is refused.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchIndexRequest {
    read_only: bool,
}

/// What `PUT /v1/indexes/{index}` carries (M147). Anything else is refused.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    dims: u64,
    distance_metric: Option<String>,
}

/// A write body, with the durability level refused by name rather than by serde's message.
///
/// ⚠️ Parsed once into its type where that is the tree's answer (M120, [`body`]): a tree holds
/// each number of each vector as a 32-byte node, and was walked again to build the request.
/// A body that parses so cannot hold a durability the refusal below names: the type has no
/// such variant.
///
/// With it, how many keys the body's object holds (M122): a branch must come alone, and
/// counting them parsed the body a second time, as a tree.
fn parse_write(body: &[u8]) -> Result<(WriteRequest, usize), ApiError> {
    if let Some((shape, req)) = body::typed::<WriteRequest>(body) {
        // A body that parses as a request is an object, so `plain` named its keys.
        return Ok((req, shape.keys.map_or(0, |k| k.len())));
    }
    let raw: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request(format!("malformed JSON: {e}")))?;
    let keys = raw.as_object().map_or(0, serde_json::Map::len);
    if let Some(d) = raw.get("durability").and_then(serde_json::Value::as_str)
        && !matches!(d, "batched" | "durable" | "applied")
    {
        // ⚠️ **Refused, never downgraded.** Accepting `async` and treating it as `batched`
        // is a promise about durability that the system does not keep, which is the one
        // class of lie a storage system may never tell.
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "unsupported_durability",
            format!(
                "durability {d:?} is not implemented; use \"batched\", \"durable\" or \"applied\""
            ),
        ));
    }
    serde_json::from_value(raw)
        .map(|req| (req, keys))
        .map_err(|e| ApiError::bad_request(format!("malformed body: {e}")))
}

/// The most a `rank_by` query may skip and return together (M9e): its rows are held in memory.
const MAX_ORDERED: usize = 10_000;

/// The order a query asks for, if it asks for one -- or why it cannot mean one (M9e).
///
/// ⚠️ **Refused, never combined.** An ordered answer and a relevance answer are different
/// questions, so `rank_by` beside `vector`, `text`, `field` or `exact: true` is a request that
/// means nothing -- and answering one of them would silently drop the other.
///
/// `rank_by` is one `[attribute, "asc" | "desc"]` pair, or a list of 1 to 8 of them (M140): an
/// array whose first element is itself an array is the list.
fn order_by(req: &QueryRequest) -> Result<Option<pstore_query::Order>, ApiError> {
    // M163: a relevance ranking takes `offset` too; `plan` bounds its page.
    let Some(raw) = &req.rank_by else {
        return Ok(None);
    };
    let by = match raw.as_array() {
        Some(list) if list.is_empty() || list.first().is_some_and(serde_json::Value::is_array) => {
            pstore_query::Order::new(list.iter().map(order_key).collect::<Result<_, _>>()?)
                .map_err(ApiError::bad_request)?
        }
        _ => order_key(raw)?.into(),
    };
    if req.vector.is_some()
        || req.multi.is_some()
        || req.text.is_some()
        || req.sparse.is_some()
        || req.field.is_some()
        || req.exact
        || req.fusion.is_some()
    {
        return Err(ApiError::bad_request(
            "rank_by orders by an attribute; it cannot be combined with vector, multi, text, \
             sparse, field, exact or fusion",
        ));
    }
    if req.top_k.saturating_add(req.offset.unwrap_or(0)) > MAX_ORDERED {
        return Err(ApiError::bad_request(format!(
            "top_k + offset may be at most {MAX_ORDERED} for a rank_by order"
        )));
    }
    Ok(Some(by))
}

/// One `[attribute, "asc" | "desc"]` pair of `rank_by`.
fn order_key(pair: &serde_json::Value) -> Result<pstore_query::OrderBy, ApiError> {
    let Some(
        [
            serde_json::Value::String(attr),
            serde_json::Value::String(dir),
        ],
    ) = pair.as_array().map(Vec::as_slice)
    else {
        return Err(ApiError::bad_request(format!(
            "rank_by is [attribute, \"asc\" | \"desc\"], or a list of 1 to {} of them",
            pstore_query::MAX_KEYS
        )));
    };
    let desc = match dir.as_str() {
        "asc" => false,
        "desc" => true,
        other => {
            return Err(ApiError::bad_request(format!(
                "rank_by direction {other:?} is not \"asc\" or \"desc\""
            )));
        }
    };
    Ok(pstore_query::OrderBy {
        attr: attr.clone(),
        desc,
    })
}

/// The legs a query asks for. A query with none is a request nobody meant to make.
fn prefetch(req: &QueryRequest, limit: usize) -> Result<Vec<pstore_query::Prefetch>, ApiError> {
    let mut legs = Vec::new();
    // M77: a named field is scanned exactly over its own vectors. A name the engine reserves
    // (the derived encoding's) is refused here, as `multi`'s is.
    let dense_field = match &req.field {
        Some(f) if f != pstore_format::DEFAULT_FIELD => named_field(f)?,
        _ => pstore_format::DEFAULT_FIELD.to_owned(),
    };
    if let Some(v) = &req.vector {
        if v.is_empty() {
            return Err(ApiError::bad_request("an empty query vector"));
        }
        legs.push(pstore_query::Prefetch::Dense {
            field: dense_field,
            query: v.clone(),
            limit,
            tune: pstore_index::vec_index::Query {
                exact: req.exact,
                ..pstore_index::vec_index::Query::default()
            },
        });
    }
    if let Some(m) = &req.multi {
        let field = named_field(&m.field)?;
        let query = many(m.vectors.clone())
            .map_err(|why| ApiError::bad_request(format!("multi: {why}")))?;
        let fde = match m.mode.as_ref() {
            None => false,
            Some(v) => match v.as_str() {
                Some("exact") => false,
                Some("fde") => true,
                _ => {
                    return Err(ApiError::bad_request("multi: mode is \"exact\" or \"fde\""));
                }
            },
        };
        let rerank = multi_rerank(m, fde, limit, req.filters.is_some())?;
        let probes = multi_probes(m, fde)?;
        if fde {
            legs.push(pstore_query::Prefetch::MultiFde {
                field,
                query,
                limit,
                rerank,
                probes,
            });
        } else {
            legs.push(pstore_query::Prefetch::Multi {
                field,
                query,
                limit,
            });
        }
    }
    let texts = req.text.as_ref().map(TextIn::legs).transpose();
    let texts = texts.map_err(ApiError::bad_request)?.unwrap_or_default();
    if req.text.is_some() && !(1..pstore_query::MAX_LEGS).contains(&texts.len()) {
        return Err(ApiError::bad_request(format!(
            "text is a string or an array of 1 to {} strings or {{\"field\", \"query\"}} \
             objects",
            pstore_query::MAX_LEGS - 1
        )));
    }
    // M155: an object names its field; a string is the primary's.
    for (field, t, last_as_prefix) in texts {
        legs.push(pstore_query::Prefetch::Text {
            field: field
                .unwrap_or(pstore_format::text::DEFAULT_TEXT_FIELD)
                .to_owned(),
            query: t,
            limit,
            last_as_prefix,
        });
    }
    // M178: after the text legs, so a positional fusion weight names it last.
    if let Some(s) = &req.sparse {
        let query =
            sparse_pairs(s).map_err(|why| ApiError::bad_request(format!("sparse: {why}")))?;
        legs.push(pstore_query::Prefetch::Sparse {
            field: SPARSE_FIELD.to_owned(),
            query,
            limit,
        });
    }
    if legs.is_empty() {
        return Err(ApiError::bad_request(
            "a query must name a vector, multi, text, sparse, or several",
        ));
    }
    if legs.len() > pstore_query::MAX_LEGS {
        return Err(ApiError::bad_request(format!(
            "{} legs: a query has at most {}",
            legs.len(),
            pstore_query::MAX_LEGS
        )));
    }
    Ok(legs)
}

/// Named vector fields a document holds at most (M61).
const MAX_NAMED_FIELDS: usize = 8;
/// Vectors a named field holds at most, in a document or a query (M61).
const MAX_FIELD_VECTORS: usize = 1024;
/// Bytes a named field's name holds at most (M61).
const MAX_FIELD_NAME: usize = 64;

/// The rerank window of an `fde` multi leg, or `None` for none (M67).
fn multi_rerank(
    m: &types::MultiIn,
    fde: bool,
    limit: usize,
    filtered: bool,
) -> Result<Option<usize>, ApiError> {
    let bad = |why: &str| ApiError::bad_request(format!("multi: {why}"));
    let rerank = match m.rerank.as_ref() {
        None => false,
        Some(v) if v.as_str() == Some("exact") => true,
        Some(_) => return Err(bad("rerank is \"exact\"")),
    };
    if !rerank {
        return if m.rerank_window.is_some() {
            Err(bad("rerank_window needs rerank"))
        } else {
            Ok(None)
        };
    }
    if !fde {
        return Err(bad(
            "rerank re-scores an fde leg's candidates: it needs mode \"fde\"",
        ));
    }
    // ⚠️ The mask is applied after the leg: a rerank would re-score candidates the filter
    // then drops, and the survivors would carry the encoding's scores.
    if filtered {
        return Err(bad("rerank cannot be combined with filters"));
    }
    // ⚠️ Before the default's `clamp`, which panicked on a depth past the bound (M163).
    if limit > boost::MAX_WINDOW {
        return Err(bad(&format!(
            "rerank reaches at most {} candidates; the leg's depth is {limit}",
            boost::MAX_WINDOW
        )));
    }
    let window = match m.rerank_window.as_ref() {
        None => limit.saturating_mul(4).clamp(limit, boost::MAX_WINDOW),
        Some(w) => w
            .as_u64()
            .and_then(|w| usize::try_from(w).ok())
            .filter(|w| (limit..=boost::MAX_WINDOW).contains(w))
            .ok_or_else(|| {
                bad(&format!(
                    "rerank_window is a whole number from the leg's limit ({limit}) to {}",
                    boost::MAX_WINDOW
                ))
            })?,
    };
    Ok(Some(window))
}

/// The clustered lists an `fde` multi leg reads, or `None` for the default (M72).
fn multi_probes(m: &types::MultiIn, fde: bool) -> Result<Option<usize>, ApiError> {
    let Some(v) = m.probes.as_ref() else {
        return Ok(None);
    };
    let bad = |why: &str| ApiError::bad_request(format!("multi: {why}"));
    if !fde {
        return Err(bad("probes needs mode \"fde\""));
    }
    v.as_u64()
        .and_then(|p| usize::try_from(p).ok())
        .filter(|p| (1..=MAX_PROBES).contains(p))
        .map(Some)
        .ok_or_else(|| bad(&format!("probes is a whole number from 1 to {MAX_PROBES}")))
}

/// Lists an `fde` leg may be told to read (M72).
const MAX_PROBES: usize = 1024;

/// A named field's name, or why it cannot be one (M61): `vector` is the dense field's.
fn named_field(name: &str) -> Result<String, ApiError> {
    if name.contains('\0') {
        return Err(ApiError::bad_request(
            "a named vector field's name may not contain a NUL byte: it is reserved",
        ));
    }
    if name.ends_with(pstore_format::fde::FIELD_SUFFIX) {
        return Err(ApiError::bad_request(format!(
            "a named vector field's name may not end `{}`: that suffix holds the encoding the \
             engine derives",
            pstore_format::fde::FIELD_SUFFIX
        )));
    }
    if name.is_empty() || name.len() > MAX_FIELD_NAME {
        return Err(ApiError::bad_request(format!(
            "a named vector field's name is 1 to {MAX_FIELD_NAME} bytes"
        )));
    }
    if name == pstore_format::DEFAULT_FIELD {
        return Err(ApiError::bad_request(
            "`vector` is the dense field: a named vector field takes another name",
        ));
    }
    if name == SPARSE_FIELD {
        return Err(ApiError::bad_request(
            "`sparse` is the sparse field (M178): a named vector field takes another name",
        ));
    }
    Ok(name.to_owned())
}

/// A named field's vectors, or why they cannot be (M61): 1 to [`MAX_FIELD_VECTORS`], none
/// empty. Non-finite components are refused as `vector`'s are, when they are read.
fn many(vs: Vec<Vec<f32>>) -> Result<Vec<Vec<f32>>, String> {
    if !(1..=MAX_FIELD_VECTORS).contains(&vs.len()) {
        return Err(format!(
            "1 to {MAX_FIELD_VECTORS} vectors, not {}",
            vs.len()
        ));
    }
    if vs.iter().any(Vec::is_empty) {
        return Err("an empty vector".to_owned());
    }
    Ok(vs)
}

/// The wire document, as the format's — or the reason it cannot be one.
///
/// ⚠️ **Refused, never coerced** (M9a). The format stores `i64`, strings, and since M9h.1
/// finite floats and bools; turning `1.5` into `1`, `true` into `"true"`, or `null` into an
/// absent attribute would store a value the client did not write, and nothing downstream
/// could tell.
/// The attribute types a write declares (M9h.3, M171), or why its `schema` is refused.
///
/// ⚠️ A declaration that would do nothing is refused rather than ignored: another type, a
/// name no document uses, or the text field -- which is always a string -- would each
/// answer 200 and store something other than what the client meant.
fn declared_types(req: &WriteRequest) -> Result<typed::Declarations, ApiError> {
    let Some(schema) = &req.schema else {
        return Ok(typed::Declarations::new());
    };
    let bad = |why: String| ApiError::bad_request(format!("schema: {why}"));
    let schema = schema
        .as_object()
        .ok_or_else(|| bad("an object of attribute names to types".to_owned()))?;
    let mut out = typed::Declarations::new();
    for (name, ty) in schema {
        // M176: the ids' kind, which no document carries as an attribute; `string` is the
        // default, so it declares nothing to remember.
        // An object (full text, regex) falls through, to be refused as any reserved name is.
        if name == pstore_query::ID_ATTRIBUTE && !ty.is_object() {
            match ty.as_str() {
                Some("string") => {}
                Some("uint64") => _ = out.insert(name.clone(), pstore_format::typed::Kind::Uint64),
                Some("uuid") => _ = out.insert(name.clone(), pstore_format::typed::Kind::Uuid),
                _ => {
                    return Err(bad(format!(
                        "\"id\" is declared {ty}; an id is string, uint64 or uuid"
                    )));
                }
            }
            continue;
        }
        // An object naming `full_text_search` is a full-text declaration (M14), which
        // `declared_fts` reads; any other object is refused below, with a datetime's message.
        if ty.get("full_text_search").is_some() || ty.get("regex").is_some() {
            continue;
        }
        let Some(declared) = typed::named(ty) else {
            return Err(bad(format!(
                "{name:?} is declared {ty}; a write declares datetime, uuid, bytes or uint64"
            )));
        };
        if name == pstore_format::text::DEFAULT_TEXT_FIELD {
            return Err(bad(format!("{name:?} is the text field, always a string")));
        }
        if !req
            .documents
            .iter()
            .any(|d| d.attributes.contains_key(name))
        {
            return Err(bad(format!(
                "{name:?} is declared and no document carries it"
            )));
        }
        out.insert(name.clone(), declared);
    }
    Ok(out)
}

/// The full-text schema a write declares (M14): `{"<text field>": {"type": "string",
/// "full_text_search": true | {options}}}` in its `schema` map, or `None`.
fn declared_fts(req: &WriteRequest) -> Result<Option<pstore_format::text::FullText>, ApiError> {
    Ok(declared_texts(req)?.remove(pstore_format::text::DEFAULT_TEXT_FIELD))
}

/// Every attribute a write declares `full_text_search` on, with its `FullText` (M14, M155).
///
/// ⚠️ **One parser for the primary and every further field**, so a further field's options are
/// refused exactly as the primary's are. Which name is the primary is the caller's split.
fn declared_texts(
    req: &WriteRequest,
) -> Result<std::collections::BTreeMap<String, pstore_format::text::FullText>, ApiError> {
    let mut out = std::collections::BTreeMap::new();
    let Some(schema) = req.schema.as_ref().and_then(serde_json::Value::as_object) else {
        return Ok(out);
    };
    let bad = |why: String| ApiError::bad_request(format!("schema: {why}"));
    for (name, decl) in schema {
        let Some(decl) = decl.as_object() else {
            continue;
        };
        if let Some(k) = decl
            .keys()
            .find(|k| !["type", "full_text_search", "regex"].contains(&k.as_str()))
        {
            return Err(bad(format!("{name:?}: unknown key {k:?}")));
        }
        // M15.2: a `regex` declaration alone is `declared_trigram`'s.
        let Some(opts) = decl.get("full_text_search") else {
            continue;
        };
        if decl
            .get("type")
            .is_some_and(|t| t != "string" && t != "[]string")
        {
            return Err(bad(if name == pstore_format::text::DEFAULT_TEXT_FIELD {
                format!("{name:?}: the text field's type is string or []string")
            } else {
                format!("{name:?}: a text field's type is string or []string")
            }));
        }
        let fts = full_text_options(opts).map_err(bad)?;
        // M158: a pre-tokenized field holds an array of tokens.
        if fts.analyzer.tokenizer == pstore_format::text::Tokenizer::PreTokenizedArray
            && decl.get("type").is_some_and(|t| t == "string")
        {
            return Err(bad(format!(
                "{name:?}: a pre_tokenized_array field's type is []string"
            )));
        }
        if req.documents.is_empty() {
            return Err(bad(
                "full_text_search is declared and the write has no documents".to_owned(),
            ));
        }
        out.insert(name.clone(), fts);
    }
    Ok(out)
}

/// A `full_text_search` value (M14): `true`, or an object of options, each checked.
fn full_text_options(v: &serde_json::Value) -> Result<pstore_format::text::FullText, String> {
    use pstore_format::text::{FullText, Language, Tokenizer};
    let mut fts = FullText::default();
    match v {
        serde_json::Value::Bool(true) => {}
        serde_json::Value::Object(opts) => {
            let flag = |v: &serde_json::Value, k: &str| {
                v.as_bool()
                    .ok_or_else(|| format!("full_text_search: {k} is true or false"))
            };
            let param = |v: &serde_json::Value, k: &str, hi: f64| {
                v.as_f64()
                    .filter(|x| (0.0..=hi).contains(x))
                    // `+ 0.0` so `-0.0` is stored, and compared, as `0`.
                    .map(|x| x as f32 + 0.0)
                    .ok_or_else(|| format!("full_text_search: {k} must be a number in [0, {hi}]"))
            };
            for (k, v) in opts {
                let a = &mut fts.analyzer;
                match k.as_str() {
                    "tokenizer" => {
                        // M158: `pre_tokenized_array`, the client's own tokens; M181: `word_v0`
                        // to `word_v4`.
                        a.tokenizer = v.as_str().and_then(Tokenizer::parse).ok_or_else(|| {
                            format!(
                                "full_text_search: tokenizer {v} is not one of {}",
                                Tokenizer::ALL.map(Tokenizer::name).join(", ")
                            )
                        })?;
                    }
                    // M181: an integer, never `5.0`.
                    "max_token_length" => {
                        a.max_token_length = Some(
                            v.as_u64()
                                .and_then(|n| u8::try_from(n).ok())
                                .filter(|n| *n > 0)
                                .ok_or_else(|| {
                                    format!(
                                        "full_text_search: max_token_length {v} is not an \
                                         integer in [1, 255]"
                                    )
                                })?,
                        );
                    }
                    "language" => {
                        a.language = v.as_str().and_then(Language::parse).ok_or_else(|| {
                            format!(
                                "full_text_search: language {v} is not one of {}",
                                Language::ALL.map(Language::name).join(", ")
                            )
                        })?;
                    }
                    "stemming" => a.stemming = flag(v, k)?,
                    "remove_stopwords" => a.remove_stopwords = flag(v, k)?,
                    "case_sensitive" => a.case_sensitive = flag(v, k)?,
                    "ascii_folding" => a.ascii_folding = flag(v, k)?,
                    "k1" => fts.k1 = param(v, k, 3.0)?,
                    "b" => fts.b = param(v, k, 1.0)?,
                    _ => return Err(format!("full_text_search: unknown option {k:?}")),
                }
            }
            if fts.analyzer.remove_stopwords && fts.analyzer.language != Language::English {
                return Err("full_text_search: remove_stopwords is English only".to_owned());
            }
            // M158: a pre-tokenized token is kept byte for byte, so nothing may change it --
            // and it is matched case-sensitively, whatever was declared.
            if fts.analyzer.tokenizer == Tokenizer::PreTokenizedArray {
                let a = &mut fts.analyzer;
                let changes = [
                    ("language", opts.contains_key("language")),
                    ("stemming: true", a.stemming),
                    ("remove_stopwords: true", a.remove_stopwords),
                    (
                        "case_sensitive: false",
                        opts.get("case_sensitive") == Some(&serde_json::Value::Bool(false)),
                    ),
                    ("ascii_folding: true", a.ascii_folding),
                    // M181: a token is kept whole, so no limit can drop it.
                    ("max_token_length", a.max_token_length.is_some()),
                ];
                if let Some((what, _)) = changes.iter().find(|(_, set)| *set) {
                    return Err(format!(
                        "full_text_search: pre_tokenized_array keeps each token as written, \
                         so it takes no {what}"
                    ));
                }
                a.case_sensitive = true;
            }
        }
        _ => return Err("full_text_search is true, or an object of options".to_owned()),
    }
    Ok(fts)
}

/// The further text fields a write declares (M155): every `full_text_search` but the
/// primary's, or `None` when it declares none.
fn declared_further(
    req: &WriteRequest,
) -> Result<Option<std::collections::BTreeMap<String, pstore_format::text::FullText>>, ApiError> {
    let mut all = declared_texts(req)?;
    all.remove(pstore_format::text::DEFAULT_TEXT_FIELD);
    Ok((!all.is_empty()).then_some(all))
}

/// Refuses a document of the declaring write whose further text field holds something other
/// than a string or an array of strings (M155), as `to_document` refuses the primary's (M150).
fn check_further(
    docs: &[Document],
    texts: Option<&std::collections::BTreeMap<String, pstore_format::text::FullText>>,
) -> Result<(), ApiError> {
    let Some(texts) = texts else {
        return Ok(());
    };
    for d in docs {
        for name in texts.keys() {
            let text = match d.attrs.get(name) {
                None | Some(pstore_format::Value::Str(_)) => true,
                Some(pstore_format::Value::Array(items)) => items
                    .iter()
                    .all(|e| matches!(e, pstore_format::Value::Str(_))),
                Some(_) => false,
            };
            if !text {
                return Err(ApiError::bad_request(format!(
                    "document {}: attribute `{name}` is a text field and must be a string or \
                     an array of strings",
                    d.id
                )));
            }
        }
    }
    Ok(())
}

/// M158: in a write that declares a `pre_tokenized_array` field, each document holds an array
/// of strings there, or nothing, and no string holds U+001F -- the one character a query
/// token cannot, so a token holding it could be stored and never asked for. A write that
/// does not declare the field is not checked here; its strings are tokens whole.
fn check_pre_tokenized(
    docs: &[Document],
    primary: Option<&pstore_format::text::FullText>,
    texts: Option<&std::collections::BTreeMap<String, pstore_format::text::FullText>>,
) -> Result<(), ApiError> {
    use pstore_format::text::{TOKEN_SEPARATOR, Tokenizer};
    let fields: Vec<&str> = primary
        .map(|f| (pstore_format::text::DEFAULT_TEXT_FIELD, f))
        .into_iter()
        .chain(texts.into_iter().flatten().map(|(n, f)| (n.as_str(), f)))
        .filter(|(_, f)| f.analyzer.tokenizer == Tokenizer::PreTokenizedArray)
        .map(|(n, _)| n)
        .collect();
    for d in docs {
        for name in &fields {
            let tokens = match d.attrs.get(*name) {
                None => true,
                // An element that is not a string is refused before this, as in any text field.
                Some(pstore_format::Value::Array(items)) => items.iter().all(
                    |e| !matches!(e, pstore_format::Value::Str(s) if s.contains(TOKEN_SEPARATOR)),
                ),
                Some(_) => false,
            };
            if !tokens {
                return Err(ApiError::bad_request(format!(
                    "document {}: attribute `{name}` is pre_tokenized_array and must be an \
                     array of strings, none holding U+001F",
                    d.id
                )));
            }
        }
    }
    Ok(())
}

/// The attributes a write declares `"regex": true` on (M15.2), sorted, or `None` for none.
/// `"regex": false` is not a declaration.
fn declared_trigram(req: &WriteRequest) -> Result<Option<Vec<String>>, ApiError> {
    let Some(schema) = req.schema.as_ref().and_then(serde_json::Value::as_object) else {
        return Ok(None);
    };
    let bad = |why: String| ApiError::bad_request(format!("schema: {why}"));
    let mut names = Vec::new();
    for (name, decl) in schema {
        let Some(flag) = decl.get("regex") else {
            continue;
        };
        let on = flag
            .as_bool()
            .ok_or_else(|| bad(format!("{name:?}: regex is true or false")))?;
        if name == pstore_query::ID_ATTRIBUTE {
            return Err(bad(
                "\"id\" cannot be declared regex: it is not sketched".to_owned()
            ));
        }
        if decl.get("type").is_some_and(|t| t != "string") {
            return Err(bad(format!("{name:?}: a regex attribute's type is string")));
        }
        if on {
            names.push(name.clone());
        }
    }
    if names.is_empty() {
        return Ok(None);
    }
    if req.documents.is_empty() {
        return Err(bad(
            "regex is declared and the write has no documents".to_owned()
        ));
    }
    names.sort();
    Ok(Some(names))
}

/// A full-text schema as `GET /v1/indexes/{index}` reports it (M14).
fn fts_json(f: &pstore_format::text::FullText) -> serde_json::Value {
    let a = &f.analyzer;
    let mut out = serde_json::json!({
        "tokenizer": a.tokenizer.name(),
        "language": a.language.name(),
        "stemming": a.stemming,
        "remove_stopwords": a.remove_stopwords,
        "case_sensitive": a.case_sensitive,
        "ascii_folding": a.ascii_folding,
        "k1": f.k1,
        "b": f.b,
    });
    // M181: reported only when set, as a field that never declared one is unchanged.
    if let (Some(max), Some(o)) = (a.max_token_length, out.as_object_mut()) {
        o.insert("max_token_length".to_owned(), max.into());
    }
    out
}

/// Refuses a batch whose documents disagree on their vector width (M170: none is a width).
///
/// ⚠️ **Refused at the door**, which is where `Engine::write` already refuses a document
/// the segment layout cannot store. A zero-dimension vector is storable and meaningless,
/// and a batch whose vectors disagree makes the *index's* dimension depend on which
/// document happened to be first. `api-design.md`: all vector dimensions in a field must
/// match — so the request that breaks that rule is the one that gets the error, rather
/// than the query that meets the consequence later.
fn widths_agree(req: &types::WriteRequest) -> Result<(), ApiError> {
    let width = |d: &types::DocumentIn| d.vector.as_ref().map(Vec::len);
    if let Some(d) = req.documents.iter().find(|d| width(d) == Some(0)) {
        return Err(ApiError::bad_request(format!(
            "document {}: an empty vector; a document of an index with no vectors omits `vector`",
            d.id
        )));
    }
    let Some(first) = req.documents.first() else {
        return Ok(());
    };
    let dims = width(first);
    if let Some(odd) = req.documents.iter().find(|d| width(d) != dims) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "schema_conflict",
            format!(
                "document {} has {} and the batch has {}",
                odd.id,
                dims_named(width(odd)),
                dims_named(dims)
            ),
        ));
    }
    if dims.is_none() {
        vector_free(req)?;
    }
    Ok(())
}

/// A width as a refusal names it.
fn dims_named(dims: Option<usize>) -> String {
    dims.map_or_else(|| "no vector".to_owned(), |n| format!("{n} dimensions"))
}

/// Refuses what a batch of documents with no vector cannot carry (M170): named vector
/// fields, which need the dense field beside them, and a distance metric, which has nothing
/// to measure.
fn vector_free(req: &types::WriteRequest) -> Result<(), ApiError> {
    if let Some(d) = req.documents.iter().find(|d| !d.vectors.is_empty()) {
        return Err(ApiError::bad_request(format!(
            "document {}: named vector fields need a `vector` beside them",
            d.id
        )));
    }
    if req.distance_metric.is_some() {
        return Err(ApiError::bad_request(
            "distance_metric on a write whose documents carry no vector",
        ));
    }
    Ok(())
}

///
/// ⚠️ Takes the document by value (M119): its vectors move into the format's, where a borrow
/// copied every one of them.
fn to_document(d: types::DocumentIn, dated: &typed::Declarations) -> Result<Document, ApiError> {
    let text_field = pstore_format::text::DEFAULT_TEXT_FIELD;
    let types::DocumentIn {
        id,
        vector,
        vectors,
        text,
        sparse,
        attributes,
    } = d;
    let mut doc = Document::new(id.0.clone(), vector.unwrap_or_default());
    let refuse = |name: &str, why: &str| {
        ApiError::bad_request(format!("document {id}: attribute `{name}` {why}"))
    };
    for (name, value) in &attributes {
        if let Some(why) = name_refusal(name) {
            return Err(refuse(name, why));
        }
        let v = match (dated.get(name), value) {
            // M9h.3, M171: a declared type is read from a string, or an array of them.
            (Some(d), _) => typed::read(*d, value),
            // M9h.2: an array of scalars; an element that is not one is refused, never dropped.
            // M175: an element may be null; a filter literal may not, so not `scalar`'s.
            (None, serde_json::Value::Array(items)) => items
                .iter()
                .map(|i| match i {
                    serde_json::Value::Null => Ok(pstore_format::Value::Null),
                    _ => scalar(i).map_err(|why| format!("holds an element that {why}")),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(pstore_format::Value::Array),
            (None, _) => scalar(value),
        };
        let v = match v {
            Ok(v) => v,
            Err(why) => return Err(refuse(name, &why)),
        };
        // M150: a string, or an array of strings, every element one.
        let text = match &v {
            pstore_format::Value::Str(_) => true,
            pstore_format::Value::Array(items) => items
                .iter()
                .all(|e| matches!(e, pstore_format::Value::Str(_))),
            _ => false,
        };
        if name == text_field && !text {
            return Err(refuse(
                name,
                "is the text field and must be a string or an array of strings",
            ));
        }
        doc.attrs.insert(name.clone(), v);
    }
    if vectors.len() > MAX_NAMED_FIELDS {
        return Err(ApiError::bad_request(format!(
            "document {}: {} named vector fields, at most {MAX_NAMED_FIELDS}",
            id,
            vectors.len()
        )));
    }
    for (name, vs) in vectors {
        let name = named_field(&name)
            .map_err(|e| ApiError::bad_request(format!("document {}: {}", id, e.message)))?;
        let vs = many(vs).map_err(|why| {
            ApiError::bad_request(format!("document {id}: field `{name}`: {why}"))
        })?;
        doc.vectors
            .insert(name.into(), pstore_format::VectorField::Dense(vs));
    }
    if let Some(s) = sparse {
        let pairs = sparse_pairs(&s)
            .map_err(|why| ApiError::bad_request(format!("document {id}: sparse: {why}")))?;
        doc.vectors.insert(
            SPARSE_FIELD.into(),
            pstore_format::VectorField::Sparse(
                pairs
                    .into_iter()
                    .map(|(k, w)| (k, pstore_format::Impact::new(w)))
                    .collect(),
            ),
        );
    }
    if let Some(t) = text {
        // `attributes.text` and top-level `text` are one field spelled twice; which one
        // wins would be a rule nobody wrote down.
        if doc.attrs.contains_key(text_field) {
            return Err(refuse(
                text_field,
                "is given twice: as `text` and in `attributes`",
            ));
        }
        doc.attrs
            .insert(text_field.to_owned(), pstore_format::Value::Str(t));
    }
    Ok(doc)
}

/// A turbopuffer filter array as a predicate — or why it is not one (M9b).
///
/// ⚠️ **Refused, never guessed.** A clause this cannot read is a 400, not dropped: a filter
/// silently ignored answers with documents the caller excluded.
fn predicate(v: &serde_json::Value) -> Result<pstore_query::Predicate, ApiError> {
    let p = clause(v)?;
    // M15: every pattern is compiled, and a regex may compile to a megabyte.
    if p.patterns() > MAX_PATTERNS {
        return Err(ApiError::bad_request(format!(
            "malformed filter: a filter holds at most {MAX_PATTERNS} patterns, and this holds {}",
            p.patterns()
        )));
    }
    Ok(p)
}

/// The one sparse field a document carries and a query searches (M178). A name of its own
/// rather than one a caller picks: a segment holds one sparse field's postings (M177).
const SPARSE_FIELD: &str = "sparse";
/// The most keys a sparse vector holds, a document's or a query's.
const MAX_SPARSE_KEYS: usize = 1_024;
/// The longest sparse key, in bytes.
const MAX_SPARSE_KEY: usize = 256;
/// The largest sparse weight's magnitude: f16's largest finite value, as a weight is stored
/// in f16 (M178), and a larger one would be infinite.
const MAX_SPARSE_WEIGHT: f64 = 65_504.0;

/// A sparse vector's `(key, weight)` pairs, or why `v` is not one (M178): an object of 1 to
/// [`MAX_SPARSE_KEYS`] keys of 1 to [`MAX_SPARSE_KEY`] bytes, each weight a number no larger in
/// magnitude than [`MAX_SPARSE_WEIGHT`]. A key spelled twice is its last weight, as a JSON
/// object is read everywhere here.
fn sparse_pairs(v: &serde_json::Value) -> Result<Vec<(String, f32)>, String> {
    let serde_json::Value::Object(m) = v else {
        return Err("an object of key to weight".to_owned());
    };
    if m.is_empty() || m.len() > MAX_SPARSE_KEYS {
        return Err(format!(
            "{} keys: a sparse vector holds 1 to {MAX_SPARSE_KEYS}",
            m.len()
        ));
    }
    m.iter()
        .map(|(k, w)| {
            if k.is_empty() || k.len() > MAX_SPARSE_KEY {
                return Err(format!(
                    "a key of {} bytes: a key is 1 to {MAX_SPARSE_KEY}",
                    k.len()
                ));
            }
            match w.as_f64() {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "bounded by f16's range, inside f32's"
                )]
                Some(x) if x.abs() <= MAX_SPARSE_WEIGHT => Ok((k.clone(), x as f32)),
                _ => Err(format!(
                    "key {k:?}'s weight {w} is not a number of magnitude at most \
                     {MAX_SPARSE_WEIGHT}"
                )),
            }
        })
        .collect()
}

/// The most `Glob`, `IGlob`, `Regex` and `Fuzzy` clauses one filter may hold (M15).
const MAX_PATTERNS: usize = 16;

/// One clause of a filter, recursively: [`predicate`] without the pattern cap.
fn clause(v: &serde_json::Value) -> Result<pstore_query::Predicate, ApiError> {
    use pstore_query::{Op, Predicate};
    let bad = |why: String| ApiError::bad_request(format!("malformed filter {v}: {why}"));
    let arr = v
        .as_array()
        .ok_or_else(|| bad("a filter is an array".to_owned()))?;
    match arr.as_slice() {
        [op, clauses] if op == "And" || op == "Or" => {
            let list = clauses
                .as_array()
                .ok_or_else(|| bad(format!("{op} takes an array of filters")))?;
            let parts = list.iter().map(clause).collect::<Result<Vec<_>, _>>()?;
            Ok(if op == "And" {
                Predicate::And(parts)
            } else {
                Predicate::Or(parts)
            })
        }
        [op, inner] if op == "Not" => Ok(Predicate::Not(Box::new(clause(inner)?))),
        // M145: the vendor's `Fuzzy`: a query string and its options, the distance chosen here
        // by the query's length in characters. Below every threshold it matches nothing.
        [attr, op, value, opts] if op == "Fuzzy" => {
            let q = value
                .as_str()
                .filter(|q| q.chars().count() <= 256)
                .ok_or_else(|| {
                    bad("Fuzzy's query is a string of at most 256 characters".to_owned())
                })?;
            let o =
                opts.as_object()
                    .filter(|o| {
                        o.keys()
                            .all(|k| k == "max_edit_distance" || k == "case_sensitive")
                    })
                    .ok_or_else(|| {
                        bad("Fuzzy's options are {\"max_edit_distance\": [..], \"case_sensitive\": \
                         bool}"
                        .to_owned())
                    })?;
            let case = match o.get("case_sensitive") {
                None => true,
                Some(b) => b
                    .as_bool()
                    .ok_or_else(|| bad("case_sensitive is true or false".to_owned()))?,
            };
            let steps = o
                .get("max_edit_distance")
                .and_then(serde_json::Value::as_array)
                .filter(|s| (1..=8).contains(&s.len()))
                .ok_or_else(|| {
                    bad("Fuzzy's max_edit_distance is a list of 1 to 8 entries".to_owned())
                })?;
            let n = q.chars().count() as u64;
            let (mut k, mut floor) = (None, None);
            for e in steps {
                let field = |name: &str| {
                    e.as_object()
                        .filter(|e| e.len() == 2)
                        .and_then(|e| e.get(name))
                        .and_then(serde_json::Value::as_u64)
                };
                let (Some(m), Some(d)) = (field("min_query_chars"), field("distance")) else {
                    return Err(bad(format!(
                        "{e}: an entry is {{\"min_query_chars\": n, \"distance\": d}}"
                    )));
                };
                let d = u8::try_from(d)
                    .ok()
                    .filter(|d| *d <= 2 && m >= 3 * (u64::from(*d) + 1));
                let Some(d) = d else {
                    return Err(bad(format!(
                        "{e}: a distance is 0, 1 or 2, with min_query_chars at least \
                         3 * (distance + 1)"
                    )));
                };
                if floor.is_some_and(|f| m <= f) {
                    return Err(bad(
                        "max_edit_distance's min_query_chars strictly increase".to_owned()
                    ));
                }
                floor = Some(m);
                if m <= n {
                    k = Some(d);
                }
            }
            // The attribute is checked as every clause's is, reserved names included.
            let p = clause(&serde_json::json!([attr, op, {"value": q, "max_edits": 0}]))?;
            let Predicate::Fuzzy(a, v, _) = p else {
                return Err(bad("Fuzzy takes a query string".to_owned()));
            };
            Ok(k.map_or(Predicate::Or(Vec::new()), |k| {
                Predicate::FuzzySubstring(a, v, k, case)
            }))
        }
        // M138: the vendor's options element, on the two token operators that take one. Its
        // only key is `last_as_prefix`; `{}` and `false` mean the three-element clause.
        [attr, op, value, opts] => {
            if op != "ContainsAllTokens" && op != "ContainsAnyToken" {
                return Err(bad(format!(
                    "{op} takes no options: only ContainsAllTokens, ContainsAnyToken and Fuzzy do"
                )));
            }
            let o = opts
                .as_object()
                .filter(|o| o.keys().all(|k| k == "last_as_prefix"))
                .ok_or_else(|| bad(format!("{op}'s options are {{\"last_as_prefix\": bool}}")))?;
            let prefix = match o.get("last_as_prefix") {
                None => false,
                Some(b) => b
                    .as_bool()
                    .ok_or_else(|| bad("last_as_prefix is true or false".to_owned()))?,
            };
            let p = clause(&serde_json::json!([attr, op, value]))?;
            Ok(match p {
                Predicate::Tokens(a, how, text, b) if prefix => {
                    let how = match how {
                        pstore_query::TokenOp::All => pstore_query::TokenOp::AllPrefix,
                        _ => pstore_query::TokenOp::AnyPrefix,
                    };
                    Predicate::Tokens(a, how, text, b)
                }
                p => p,
            })
        }
        [attr, op, value] => {
            let attr = attr
                .as_str()
                .ok_or_else(|| bad("the attribute is a string".to_owned()))?
                .to_owned();
            // M40: a reserved name -- `$` metadata, or the empty tombstone mark -- holds nothing
            // a filter can see: it is stripped before anything is sealed or served, and a
            // write's condition judged it against stamps on rows still unfolded in its fold.
            if attr.is_empty() || attr.starts_with('$') {
                return Err(bad(format!(
                    "attribute {attr:?} is reserved: names beginning `$`, and the empty name, \
                     are metadata no filter may name"
                )));
            }
            let op = op
                .as_str()
                .ok_or_else(|| bad("the operator is a string".to_owned()))?;
            let scalar =
                |x: &serde_json::Value| literal(x).map_err(|why| bad(format!("{x} {why}")));
            let cmp = |o: Op| -> Result<Predicate, ApiError> {
                Ok(Predicate::Cmp(attr.clone(), o, scalar(value)?))
            };
            match op {
                "Eq" if value.is_null() => Ok(Predicate::Absent(attr)),
                "NotEq" if value.is_null() => Ok(Predicate::Not(Box::new(Predicate::Absent(attr)))),
                "Eq" => cmp(Op::Eq),
                "NotEq" => Ok(Predicate::Not(Box::new(cmp(Op::Eq)?))),
                "Lt" => cmp(Op::Lt),
                "Lte" => cmp(Op::Lte),
                "Gt" => cmp(Op::Gt),
                "Gte" => cmp(Op::Gte),
                // M135: an array holding an element in that order to the literal.
                "AnyLt" => Ok(Predicate::AnyCmp(attr, Op::Lt, scalar(value)?)),
                "AnyLte" => Ok(Predicate::AnyCmp(attr, Op::Lte, scalar(value)?)),
                "AnyGt" => Ok(Predicate::AnyCmp(attr, Op::Gt, scalar(value)?)),
                "AnyGte" => Ok(Predicate::AnyCmp(attr, Op::Gte, scalar(value)?)),
                // M9h.2: `Contains x` is `ContainsAny [x]`, over an array attribute's elements.
                "Contains" | "NotContains" => {
                    let p = Predicate::ContainsAny(attr, vec![scalar(value)?]);
                    Ok(if op == "Contains" {
                        p
                    } else {
                        Predicate::Not(Box::new(p))
                    })
                }
                "ContainsAny" | "NotContainsAny" => {
                    let set = value
                        .as_array()
                        .ok_or_else(|| bad(format!("{op} takes an array of values")))?
                        .iter()
                        .map(scalar)
                        .collect::<Result<Vec<_>, _>>()?;
                    let p = Predicate::ContainsAny(attr, set);
                    Ok(if op == "ContainsAny" {
                        p
                    } else {
                        Predicate::Not(Box::new(p))
                    })
                }
                "In" | "NotIn" => {
                    let set = value
                        .as_array()
                        .ok_or_else(|| bad(format!("{op} takes an array of values")))?
                        .iter()
                        .map(scalar)
                        .collect::<Result<Vec<_>, _>>()?;
                    let p = Predicate::In(attr, set);
                    Ok(if op == "In" {
                        p
                    } else {
                        Predicate::Not(Box::new(p))
                    })
                }
                // M14.2: token predicates, bound to the index's analyzer by the engine.
                "ContainsAllTokens" | "ContainsAnyToken" | "ContainsTokenSequence" => {
                    // M158: or an array of strings, the tokens of a pre-tokenized field.
                    let text =
                        types::token_operand(value).map_err(|why| bad(format!("{op}: {why}")))?;
                    let how = match op {
                        "ContainsAllTokens" => pstore_query::TokenOp::All,
                        "ContainsAnyToken" => pstore_query::TokenOp::Any,
                        _ => pstore_query::TokenOp::Sequence,
                    };
                    Ok(Predicate::Tokens(attr, how, text, None))
                }
                // M15: patterns, compiled here once; a refusal names the operator.
                "Glob" | "NotGlob" | "IGlob" | "NotIGlob" | "Regex" => {
                    let source = value
                        .as_str()
                        .ok_or_else(|| bad(format!("{op} takes a string pattern")))?;
                    let kind = match op {
                        "Glob" | "NotGlob" => pstore_query::PatternKind::Glob,
                        "IGlob" | "NotIGlob" => pstore_query::PatternKind::IGlob,
                        _ => pstore_query::PatternKind::Regex,
                    };
                    let p = pstore_query::Pattern::new(kind, source)
                        .map_err(|e| bad(format!("{op}: {e}")))?;
                    let p = Predicate::Pattern(attr, p);
                    Ok(if op.starts_with("Not") {
                        Predicate::Not(Box::new(p))
                    } else {
                        p
                    })
                }
                "Fuzzy" => {
                    let o = value.as_object().filter(|o| o.len() == 2).ok_or_else(|| {
                        bad("Fuzzy takes {\"value\": <string>, \"max_edits\": 0..=2}".to_owned())
                    })?;
                    let v = o
                        .get("value")
                        .and_then(serde_json::Value::as_str)
                        .filter(|v| v.chars().count() <= 256)
                        .ok_or_else(|| {
                            bad("Fuzzy's value is a string of at most 256 characters".to_owned())
                        })?;
                    let k = o
                        .get("max_edits")
                        .and_then(serde_json::Value::as_u64)
                        .filter(|k| *k <= 2)
                        .ok_or_else(|| bad("Fuzzy's max_edits is 0, 1 or 2".to_owned()))?;
                    Ok(Predicate::Fuzzy(attr, v.into(), k as u8))
                }
                // M66: a location is a two-number array; the clause's argument is numbers.
                "GeoRadius" => {
                    let Some(&[lat, lon, meters]) = geo_numbers(value, 3).as_deref() else {
                        return Err(bad(
                            "GeoRadius takes [lat, lon, meters], three numbers".to_owned()
                        ));
                    };
                    if !geo_point(lat, lon) {
                        return Err(bad(
                            "GeoRadius's lat is -90 to 90 and lon -180 to 180".to_owned()
                        ));
                    }
                    if !(0.0..=MAX_GEO_RADIUS_M).contains(&meters) {
                        return Err(bad(format!(
                            "GeoRadius's meters is 0 to {MAX_GEO_RADIUS_M}, half the circumference"
                        )));
                    }
                    Ok(Predicate::GeoRadius(attr, lat, lon, meters))
                }
                "GeoBox" => {
                    let Some(&[south, west, north, east]) = geo_numbers(value, 4).as_deref() else {
                        return Err(bad(
                            "GeoBox takes [south, west, north, east], four numbers".to_owned()
                        ));
                    };
                    if !geo_point(south, west) || !geo_point(north, east) {
                        return Err(bad(
                            "GeoBox's latitudes are -90 to 90 and longitudes -180 to 180"
                                .to_owned(),
                        ));
                    }
                    if south > north {
                        return Err(bad("GeoBox's south is not above its north".to_owned()));
                    }
                    Ok(Predicate::GeoBox(attr, south, west, north, east))
                }
                other => Err(bad(format!("unknown operator {other}"))),
            }
        }
        _ => Err(bad(
            "expected [attr, op, value], [\"And\"|\"Or\", [..]] or [\"Not\", filter]".to_owned(),
        )),
    }
}

/// Half the Earth's circumference in meters, rounded up: the farthest two points are (M66).
const MAX_GEO_RADIUS_M: f64 = 20_015_087.0;

/// A JSON array of exactly `n` finite numbers (M66).
fn geo_numbers(v: &serde_json::Value, n: usize) -> Option<Vec<f64>> {
    let a = v.as_array().filter(|a| a.len() == n)?;
    a.iter()
        .map(|x| x.as_f64().filter(|f| f.is_finite()))
        .collect()
}

/// Whether `lat`/`lon` are a point on the globe (M66).
fn geo_point(lat: f64, lon: f64) -> bool {
    (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)
}

/// One JSON value as an attribute value, in a write or a filter -- or why it is none.
///
/// ⚠️ **The int/float split is serde_json's** (M9h.1, spec review): an `i64` is an int and an
/// `f64` a float, so `2.0` is a float -- and so is an integer literal beyond `u64` or below
/// `i64::MIN`, or `-0`, which serde_json itself parses as `f64` before this sees it. Only an
/// integer in `(i64::MAX, u64::MAX]` arrives as an integer that does not fit, and is refused.
fn scalar(x: &serde_json::Value) -> Result<pstore_format::Value, String> {
    use pstore_format::Value;
    match x {
        serde_json::Value::String(s) => Ok(Value::Str(s.clone())),
        serde_json::Value::Bool(b) => Ok(Value::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Int(i))
            } else if n.is_f64() {
                // serde_json never parses a non-finite number.
                n.as_f64()
                    .map(Value::Float)
                    .ok_or_else(|| "is not a finite number".to_owned())
            } else {
                Err("is an integer beyond i64; declare the attribute uint64 in `schema` to store it"
                    .to_owned())
            }
        }
        _ => Err("is not an integer, a float, a string or a bool, the attribute types".to_owned()),
    }
}

/// A filter's literal (M172): a write's [`scalar`], or an integer in `(i64::MAX, u64::MAX]`
/// as the unsigned number it is, which compares with every number exactly.
fn literal(x: &serde_json::Value) -> Result<pstore_format::Value, String> {
    // `scalar` refuses only such an integer and what no type holds, and only the integer
    // has a `u64`, so the refusal stands for everything else.
    scalar(x).or_else(|why| x.as_u64().map(pstore_format::Value::Uint).ok_or(why))
}

/// One attribute as JSON.
fn to_json(v: &pstore_format::Value) -> serde_json::Value {
    match v {
        pstore_format::Value::Int(n) => serde_json::Value::from(*n),
        pstore_format::Value::Str(s) => serde_json::Value::from(s.as_str()),
        // Stored floats are finite (`check_storable`), which is all `from` refuses.
        pstore_format::Value::Float(f) => serde_json::Value::from(*f),
        pstore_format::Value::Bool(b) => serde_json::Value::from(*b),
        pstore_format::Value::Array(items) => items.iter().map(to_json).collect(),
        pstore_format::Value::DateTime(t) => {
            serde_json::Value::from(pstore_format::datetime::format(*t))
        }
        pstore_format::Value::Uuid(u) => {
            serde_json::Value::from(pstore_format::typed::format_uuid(u))
        }
        pstore_format::Value::Bytes(b) => {
            serde_json::Value::from(pstore_format::typed::format_bytes(b))
        }
        pstore_format::Value::Uint(n) => serde_json::Value::from(*n),
        pstore_format::Value::Null => serde_json::Value::Null,
    }
}

/// The attributes a query asked for, or `None` when it asked for none (M9a).
///
/// ⚠️ `None` and `Some({})` are different answers: a query that did not ask gets rows with
/// no `attributes` key, exactly as before this existed; one that asked gets the key, empty
/// if the row has none of what it named.
fn selected(
    req: &QueryRequest,
    attrs: &std::collections::BTreeMap<String, pstore_format::Value>,
) -> Option<std::collections::BTreeMap<String, serde_json::Value>> {
    let include = match (&req.include_attributes, &req.exclude_attributes) {
        (None, None) | (Some(types::IncludeAttributes::All(false)), _) => return None,
        (Some(i), _) => i,
        // `exclude_attributes` alone: everything else.
        (None, Some(_)) => &types::IncludeAttributes::All(true),
    };
    let excluded = |k: &str| {
        req.exclude_attributes
            .as_ref()
            .is_some_and(|ex| ex.iter().any(|e| e == k))
    };
    Some(
        attrs
            .iter()
            .filter(|(k, _)| match include {
                types::IncludeAttributes::All(_) => true,
                types::IncludeAttributes::Named(names) => names.iter().any(|n| n == *k),
            })
            .filter(|(k, _)| !excluded(k))
            .map(|(k, v)| (k.clone(), to_json(v)))
            .collect(),
    )
}

/// What the process needs before it can serve.
///
/// ⚠️ **Read through a lookup rather than from the environment directly**, so the refusals
/// below are testable without a process-global mutation that races every other test in the
/// binary. `Config::from_env` is the one-line wiring on top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Where to listen.
    pub bind: String,
    /// This process's WAL lane.
    ///
    /// ⚠️ **Required, and never defaulted.** Lanes are single-writer and dense: two processes
    /// writing one tenant on the same lane **at once** resume at the same tail and overwrite
    /// each other's bundles — acknowledged, durable writes, gone, with no error anywhere, because
    /// a bundle is an unconditional `put`. A default here would make that the *normal* way to
    /// run two servers. Nothing detects the collision today; the backlog names create-if-absent
    /// bundles as the fix and M2's density invariant as why it is not a one-line change. A
    /// restart on the same lane, once the old process is gone, resumes it (M9j).
    pub lane: LaneId,
    /// Which store to open.
    pub backend: Backend,
    /// Which recorded profile the operator says applies to it.
    pub profile: Profile,
    /// The S3 endpoint. Required by [`Backend::S3`], ignored by [`Backend::Memory`].
    pub endpoint: String,
    /// The bucket, ignored by [`Backend::Memory`].
    pub bucket: String,
    /// The Azure account and container, present exactly when the backend is [`Backend::Azure`].
    pub azure: Option<AzureConfig>,
    /// Static credentials, or `None` to use the provider's own chain.
    ///
    /// ⚠️ **`None` is the deployed case, not the odd one.** A BYOC deployment on EC2 or EKS
    /// authenticates with an instance profile or a service-account role; baking a static key
    /// in unconditionally means such a deployment cannot authenticate at all, and an operator
    /// who forgets to set one ships a **dev password** at a real bucket. Both halves must be
    /// present or neither is used, because half a key pair is not a credential.
    pub credentials: Option<(String, String)>,
    /// The scheduled fold's policy, or `None` when `PSTORE_FOLD=off` (M9i.1).
    pub fold: Option<FoldPolicy>,
    /// The scheduled reap's policy, or `None` when `PSTORE_GC=off` (M18).
    pub gc: Option<GcPolicy>,
    /// Half of `PSTORE_GC_AGE_S` (M39), with `PSTORE_GC` on or off: how stale an engine's read
    /// of its lane may grow before a flush reads HEAD again.
    pub lane_recheck: std::time::Duration,
    /// The read cache, or `None` when `PSTORE_CACHE_DIR` is unset (M20).
    pub cache: Option<CacheConfig>,
    /// The replication worker's policy, or `None` with `PSTORE_REPLICATION=off` (M22).
    pub replication: Option<ReplicationPolicy>,
    /// The named remote stores a replication may read, from `PSTORE_SOURCES` (M22).
    pub sources: Vec<SourceConfig>,
    /// The engine registry's cap, from `PSTORE_ENGINES` (M26): 10 000 by default, `0` for none.
    pub engines: usize,
    /// The servers a query's vector legs may run on, from `PSTORE_PEERS` (M54).
    pub peers: Option<PeerConfig>,
    /// Or a SWIM membership to take them from, `PSTORE_PEER_GOSSIP_*` (M56).
    pub gossip: Option<GossipConfig>,
    /// The prefixes of customers' objects tenants may register external indexes over (M86),
    /// or `None` -- the default -- for no external index at all. From
    /// `PSTORE_EXTERNAL_SOURCES=allow` and `PSTORE_EXTERNAL_PREFIXES`.
    pub external_sources: Option<Vec<String>>,
}

/// One named remote store a replication may read (M22): read-only and S3-compatible -- GCS
/// through its S3 interoperability endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceConfig {
    /// The name a replication's `source.store` gives.
    pub name: String,
    /// `PSTORE_SOURCE_<NAME>_ENDPOINT`.
    pub endpoint: String,
    /// `PSTORE_SOURCE_<NAME>_BUCKET`.
    pub bucket: String,
    /// `PSTORE_SOURCE_<NAME>_ACCESS_KEY` and `_SECRET_KEY`, both or neither.
    pub credentials: Option<(String, String)>,
    /// `PSTORE_SOURCE_<NAME>_REGION`, `us-east-1` unset.
    pub region: String,
}

/// Where the read cache keeps its disk tier, and how large each tier is (M20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheConfig {
    /// `PSTORE_CACHE_DIR`. The tier lives in `<dir>/lane-<n>`.
    pub dir: std::path::PathBuf,
    /// `PSTORE_CACHE_RAM_BYTES`, 256 MiB by default.
    pub ram: usize,
    /// `PSTORE_CACHE_DISK_BYTES`, 4 GiB by default.
    pub disk: usize,
}

/// Where a server keeps its data.
///
/// ⚠️ **There is no `gcs`, and its absence is a decision**: `fake-gcs-server` accepts
/// `ifGenerationMatch` and ignores it, which is the worst shape a precondition can have, so it
/// is not offered and cannot be reached by an operator who has not read this comment.
///
/// **`azure` exists since M46.** C-14 records suffix ranges as absent on Azure, so a segment
/// open must not need one: since M45 HEAD records each segment's length and the open reads an
/// absolute range, and where it knows none -- an `as_of` read of a buried segment -- the
/// store's `suffix_read: false` makes it ask `head` first, two billed requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// In-process, durable for exactly as long as the process. The default M7c shipped.
    #[default]
    Memory,
    /// S3 or an S3-compatible endpoint.
    S3,
    /// Azure Blob Storage (M46), configured by [`AzureConfig`].
    Azure,
}

/// Where an Azure backend lives (M46): `PSTORE_AZURE_*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureConfig {
    /// `PSTORE_AZURE_ACCOUNT`. Required: the client builds no store without one.
    pub account: String,
    /// `PSTORE_AZURE_KEY`, or `None` for the client's own chain (managed identity) -- the
    /// deployed case, as S3's provider chain is.
    pub key: Option<String>,
    /// `PSTORE_AZURE_CONTAINER`, `pstore` unset.
    pub container: String,
    /// `PSTORE_AZURE_ENDPOINT`: the full URL up to the container, used as given with HTTP
    /// allowed -- how Azurite (`http://127.0.0.1:10000/devstoreaccount1`) and a private
    /// endpoint are reached. `None` for the account's public endpoint.
    ///
    /// ⚠️ **Never the client's emulator mode**, which ignores the endpoint and reads
    /// `AZURITE_BLOB_STORAGE_URL` instead (spec review).
    pub endpoint: Option<String>,
}

/// What the operator says the conformance suite measured about this bucket.
///
/// ⚠️ **`Unprobed` is the default and it refuses to serve.** The capability matrix is a
/// measurement; a deployment that has not made it must not accept a write it will call
/// durable. Nothing re-probes at runtime — this is the operator's word, which is the same
/// trade `scripts/conformance.sh --check` already is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// Not measured here. Both fencing primitives are reported `Divergent`.
    #[default]
    Unprobed,
    /// `scripts/conformance.sh` recorded this backend as `Supported` on CAS and
    /// create-if-absent.
    Conforming,
}

/// The capabilities an S3 backend is opened with under `profile`.
///
/// ⚠️ **A function, not a literal in `main.rs`**, because it has a branch and a branch in a
/// composition root is unreachable from `cargo test` and measured at 0% coverage. Everything
/// but the two fencing primitives is inherited from `unprobed`. ⚠️ Including
/// `delete_is_free: false`, which on real S3 is pessimistic — the matrix **declares** it
/// false for `minio`, and a value this file invented would contradict the checked-in
/// artifact it is supposed to be quoting.
#[must_use]
pub fn s3_capabilities(endpoint: &str, profile: Profile) -> pstore_blob::Capabilities {
    let base = pstore_blob::Capabilities {
        backend: format!("s3({endpoint})"),
        ..pstore_blob::ObjectStoreBackend::unprobed("s3")
    };
    match profile {
        Profile::Unprobed => base,
        Profile::Conforming => pstore_blob::Capabilities {
            cas: pstore_blob::Support::Supported,
            create_if_absent: pstore_blob::Support::Supported,
            ..base
        },
    }
}

/// The capabilities an Azure account's profile claims (M46): S3's rule, under an
/// `azure(account)` name. What the Azure client itself does -- no suffix reads, batches of
/// 256 -- is [`pstore_blob::ObjectStoreBackend::azure`]'s, whatever the profile.
#[must_use]
pub fn azure_capabilities(account: &str, profile: Profile) -> pstore_blob::Capabilities {
    pstore_blob::Capabilities {
        backend: format!("azure({account})"),
        ..s3_capabilities("", profile)
    }
}

/// The Azure store `config` names (M46), with `profile`'s capabilities.
///
/// # Errors
/// The client's refusal to build, which with an account present is a malformed endpoint.
pub fn azure_store(
    config: &AzureConfig,
    profile: Profile,
) -> Result<pstore_blob::ObjectStoreBackend, object_store::Error> {
    let mut builder = object_store::azure::MicrosoftAzureBuilder::new()
        .with_account(&config.account)
        .with_container_name(&config.container);
    if let Some(key) = &config.key {
        builder = builder.with_access_key(key);
    }
    if let Some(endpoint) = &config.endpoint {
        builder = builder
            .with_endpoint(endpoint.clone())
            .with_allow_http(true);
    }
    Ok(pstore_blob::ObjectStoreBackend::azure(
        std::sync::Arc::new(builder.build()?),
        azure_capabilities(&config.account, profile),
    ))
}

/// Why a process could not read its configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// `PSTORE_LANE` was absent or not a number.
    #[error(
        "PSTORE_LANE must be set to this process's lane number: lanes are single-writer, and \
         two servers sharing one lane overwrite each other's bundles silently"
    )]
    Lane,
    /// `PSTORE_BACKEND` named something this build does not have.
    ///
    /// ⚠️ Named rather than defaulted: falling back to `memory` on a typo is a deployment
    /// that reports success and stores nothing past the process.
    #[error(
        "PSTORE_BACKEND={0} is not a backend this build has: memory, s3, azure. GCS is \
         deliberately absent -- see docs/deploy.md"
    )]
    Backend(String),
    /// `PSTORE_PROFILE` named something that is not a recorded profile.
    #[error("PSTORE_PROFILE={0} is not a profile: unprobed, conforming")]
    Profile(String),
    /// `PSTORE_BACKEND=s3` with no endpoint.
    ///
    /// ⚠️ **Refused here rather than discovered on the first request.** An empty endpoint
    /// builds a backend named `s3()` that a `conforming` profile then declares able to fence,
    /// which moves a configuration error out of the one place that validates configuration
    /// and into request time — the failure mode this whole milestone exists to remove.
    #[error("PSTORE_S3_ENDPOINT must be set when PSTORE_BACKEND=s3")]
    Endpoint,
    /// `PSTORE_BACKEND=azure` with no account (M46), refused here for the reason
    /// [`Self::Endpoint`] is: the client would refuse it at startup anyway, further from the
    /// variable that caused it.
    #[error("PSTORE_AZURE_ACCOUNT must be set when PSTORE_BACKEND=azure")]
    AzureAccount,
    /// A scheduled-fold variable that is not what it must be (M9i.1).
    #[error("{0}={1} is refused: {2}")]
    Fold(&'static str, String, &'static str),
    /// A `PSTORE_CACHE_*` value that cannot be used (M20).
    #[error("{0}={1} is refused: {2}")]
    Cache(&'static str, String, &'static str),
    /// A `PSTORE_REPLICATION*` value that is not what it must be (M22).
    #[error("{0}={1} is refused: {2}")]
    Replication(&'static str, String, &'static str),
    /// A replication source that is not fully configured (M22).
    #[error("replication source {0:?}: {1}")]
    Source(String, &'static str),
    /// A `PSTORE_PEER*` value that cannot be used (M54).
    #[error("{0}={1} is refused: {2}")]
    Peers(&'static str, String, &'static str),
    /// `PSTORE_ENGINES` that is not a count (M26).
    #[error("PSTORE_ENGINES={0} is refused: a count of engines, 0 for unbounded")]
    Engines(String),
    /// A `PSTORE_EXTERNAL_SOURCES` that is neither `allow` nor `off` (M86).
    #[error("PSTORE_EXTERNAL_SOURCES={0} is refused: `allow` or `off`")]
    ExternalSources(String),
    /// `PSTORE_EXTERNAL_SOURCES=allow` with no prefix to allow (M86): the operator has not said
    /// which buckets a tenant may point at.
    #[error("PSTORE_EXTERNAL_SOURCES=allow needs PSTORE_EXTERNAL_PREFIXES, comma-separated")]
    ExternalPrefixes,
}

impl Config {
    /// Reads the configuration from `get`.
    ///
    /// # Errors
    /// [`ConfigError::Lane`] when `PSTORE_LANE` is absent or unparsable.
    /// ⚠️ **The lookup is a parameter**, so every refusal below is testable without a
    /// process-global mutation that races every other test in the binary — and the one line
    /// that reads the real environment lives in `main.rs`, which is wiring by the rule this
    /// crate follows.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let lane = get("PSTORE_LANE")
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or(ConfigError::Lane)?;
        let backend = match get("PSTORE_BACKEND").as_deref() {
            None | Some("memory") => Backend::Memory,
            Some("s3") => Backend::S3,
            Some("azure") => Backend::Azure,
            Some(other) => return Err(ConfigError::Backend(other.to_owned())),
        };
        let profile = match get("PSTORE_PROFILE").as_deref() {
            None | Some("unprobed") => Profile::Unprobed,
            Some("conforming") => Profile::Conforming,
            Some(other) => return Err(ConfigError::Profile(other.to_owned())),
        };
        let endpoint = get("PSTORE_S3_ENDPOINT").unwrap_or_default();
        if backend == Backend::S3 && endpoint.is_empty() {
            return Err(ConfigError::Endpoint);
        }
        let azure = match backend {
            Backend::Azure => Some(AzureConfig {
                account: get("PSTORE_AZURE_ACCOUNT")
                    .filter(|a| !a.is_empty())
                    .ok_or(ConfigError::AzureAccount)?,
                // Empty is unset, for the key and the container alike (code review): an empty key
                // is no credential, and an empty container is no container.
                key: get("PSTORE_AZURE_KEY").filter(|k| !k.is_empty()),
                container: get("PSTORE_AZURE_CONTAINER")
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| "pstore".to_owned()),
                endpoint: get("PSTORE_AZURE_ENDPOINT"),
            }),
            Backend::Memory | Backend::S3 => None,
        };
        Ok(Self {
            bind: get("PSTORE_BIND").unwrap_or_else(|| "127.0.0.1:8080".to_owned()),
            lane: LaneId(lane),
            backend,
            profile,
            endpoint,
            bucket: get("PSTORE_BUCKET").unwrap_or_else(|| "pstore".to_owned()),
            azure,
            credentials: get("PSTORE_ACCESS_KEY").zip(get("PSTORE_SECRET_KEY")),
            fold: fold_policy(&get)?,
            gc: gc_policy(&get)?,
            lane_recheck: reap_age(&get)? / 2,
            cache: cache_config(&get, backend)?,
            peers: PeerConfig::from_vars(&get)?,
            gossip: GossipConfig::from_vars(&get)?,
            replication: replication_policy(&get)?,
            sources: source_configs(&get)?,
            external_sources: external_sources(&get)?,
            engines: match get("PSTORE_ENGINES") {
                None => 10_000,
                Some(v) => v.parse().map_err(|_| ConfigError::Engines(v))?,
            },
        })
    }
}

/// The prefixes external indexes may be registered under (M86): unset and `off` are none, `allow`
/// needs the prefixes, and anything else is refused by name.
fn external_sources(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<Option<Vec<String>>, ConfigError> {
    match get("PSTORE_EXTERNAL_SOURCES").as_deref() {
        None | Some("off") => Ok(None),
        Some("allow") => {
            let prefixes: Vec<String> = get("PSTORE_EXTERNAL_PREFIXES")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect();
            if prefixes.is_empty() {
                return Err(ConfigError::ExternalPrefixes);
            }
            Ok(Some(prefixes))
        }
        Some(other) => Err(ConfigError::ExternalSources(other.to_owned())),
    }
}

/// The replication worker's policy (M22): unset is the default, `off` is none, and anything
/// else is refused by name, as the fold's values are.
fn replication_policy(
    get: &impl Fn(&str) -> Option<String>,
) -> Result<Option<ReplicationPolicy>, ConfigError> {
    let on = match get("PSTORE_REPLICATION").as_deref() {
        None => true,
        Some("off") => false,
        Some(other) => {
            return Err(ConfigError::Replication(
                "PSTORE_REPLICATION",
                other.to_owned(),
                "unset, or off to run no replication worker",
            ));
        }
    };
    let positive = |var: &'static str| -> Result<Option<u64>, ConfigError> {
        get(var)
            .map(|v| {
                v.parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or(ConfigError::Replication(var, v, "a positive integer"))
            })
            .transpose()
    };
    let d = ReplicationPolicy::default();
    let secs = std::time::Duration::from_secs;
    let shards = match positive("PSTORE_REPLICATION_SHARDS")? {
        None => d.shards,
        Some(n) => u16::try_from(n).map_err(|_| {
            ConfigError::Replication("PSTORE_REPLICATION_SHARDS", n.to_string(), "at most 65535")
        })?,
    };
    let policy = ReplicationPolicy {
        scan: positive("PSTORE_REPLICATION_SCAN_S")?.map_or(d.scan, secs),
        ttl: match positive("PSTORE_REPLICATION_TTL_S")? {
            None => d.ttl,
            // Renewed every third of it, by a loop that looks once a second.
            Some(n) if n < 3 => {
                return Err(ConfigError::Replication(
                    "PSTORE_REPLICATION_TTL_S",
                    n.to_string(),
                    "at least 3: a claim is renewed every third of it",
                ));
            }
            Some(n) => secs(n),
        },
        max: positive("PSTORE_REPLICATION_MAX")?
            .map_or(d.max, |n| usize::try_from(n).unwrap_or(usize::MAX)),
        idle: positive("PSTORE_REPLICATION_IDLE_S")?.map_or(d.idle, secs),
        shards,
        ..d
    };
    Ok(on.then_some(policy))
}

/// The named sources in `PSTORE_SOURCES` (M22), each refused by name when it lacks its
/// endpoint or bucket, has half its credentials, or repeats a name.
fn source_configs(get: &impl Fn(&str) -> Option<String>) -> Result<Vec<SourceConfig>, ConfigError> {
    let Some(names) = get("PSTORE_SOURCES") else {
        return Ok(Vec::new());
    };
    let mut out: Vec<SourceConfig> = Vec::new();
    for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(ConfigError::Source(
                name.to_owned(),
                "a source name is letters, digits and _",
            ));
        }
        if out.iter().any(|s| s.name.eq_ignore_ascii_case(name)) {
            return Err(ConfigError::Source(name.to_owned(), "named twice"));
        }
        let var = |k: &str| get(&format!("PSTORE_SOURCE_{}_{k}", name.to_ascii_uppercase()));
        let endpoint = var("ENDPOINT").ok_or(ConfigError::Source(
            name.to_owned(),
            "PSTORE_SOURCE_<NAME>_ENDPOINT is required",
        ))?;
        let bucket = var("BUCKET").ok_or(ConfigError::Source(
            name.to_owned(),
            "PSTORE_SOURCE_<NAME>_BUCKET is required",
        ))?;
        let credentials = match (var("ACCESS_KEY"), var("SECRET_KEY")) {
            (Some(k), Some(s)) => Some((k, s)),
            (None, None) => None,
            _ => {
                return Err(ConfigError::Source(
                    name.to_owned(),
                    "_ACCESS_KEY and _SECRET_KEY are given together or not at all",
                ));
            }
        };
        out.push(SourceConfig {
            name: name.to_owned(),
            endpoint,
            bucket,
            credentials,
            region: var("REGION").unwrap_or_else(|| "us-east-1".to_owned()),
        });
    }
    Ok(out)
}

/// The scheduled fold's policy from the environment (M9i.1). Unset is the default; anything
/// that is not a positive integer, or a `PSTORE_FOLD` other than `off`, is refused by name.
fn fold_policy(get: &impl Fn(&str) -> Option<String>) -> Result<Option<FoldPolicy>, ConfigError> {
    let on = match get("PSTORE_FOLD").as_deref() {
        None => true,
        Some("off") => false,
        Some(other) => {
            return Err(ConfigError::Fold(
                "PSTORE_FOLD",
                other.to_owned(),
                "unset, or off to leave folding to the operator",
            ));
        }
    };
    let positive = |var: &'static str| -> Result<Option<u64>, ConfigError> {
        get(var)
            .map(|v| {
                v.parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or(ConfigError::Fold(var, v, "a positive integer"))
            })
            .transpose()
    };
    let d = FoldPolicy::default();
    // Validated even when `off` (code review): a bad value is refused, never ignored.
    let policy = FoldPolicy {
        period: positive("PSTORE_FOLD_PERIOD_MS")?
            .map_or(d.period, std::time::Duration::from_millis),
        age: positive("PSTORE_FOLD_AGE_S")?.map_or(d.age, std::time::Duration::from_secs),
        bytes: positive("PSTORE_FOLD_BYTES")?.unwrap_or(d.bytes),
    };
    Ok(on.then_some(policy))
}

/// The read cache from `PSTORE_CACHE_DIR`, `PSTORE_CACHE_RAM_BYTES` and
/// `PSTORE_CACHE_DISK_BYTES` (M20). Each value is refused by name, as the fold's are.
fn cache_config(
    get: &impl Fn(&str) -> Option<String>,
    backend: Backend,
) -> Result<Option<CacheConfig>, ConfigError> {
    let bytes = |var: &'static str| -> Result<Option<usize>, ConfigError> {
        get(var)
            .map(|v| {
                v.parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or(ConfigError::Cache(var, v, "a positive integer"))
            })
            .transpose()
    };
    let (ram, disk) = (
        bytes("PSTORE_CACHE_RAM_BYTES")?,
        bytes("PSTORE_CACHE_DISK_BYTES")?,
    );
    let Some(dir) = get("PSTORE_CACHE_DIR") else {
        // ⚠️ A size with no directory is a setting that does nothing, and says nothing.
        for (var, v) in [
            ("PSTORE_CACHE_RAM_BYTES", ram),
            ("PSTORE_CACHE_DISK_BYTES", disk),
        ] {
            if let Some(n) = v {
                return Err(ConfigError::Cache(
                    var,
                    n.to_string(),
                    "set only with PSTORE_CACHE_DIR",
                ));
            }
        }
        return Ok(None);
    };
    if backend == Backend::Memory {
        // ⚠️ A memory store is new and empty at every start, and its epochs restart: its keys
        // repeat with other bytes, which a cache that survives the restart would serve.
        return Err(ConfigError::Cache(
            "PSTORE_CACHE_DIR",
            dir,
            "not with PSTORE_BACKEND=memory, whose keys repeat across restarts",
        ));
    }
    Ok(Some(CacheConfig {
        dir: dir.into(),
        ram: ram.unwrap_or(256 << 20),
        disk: disk.unwrap_or(4 << 30),
    }))
}

/// The scheduled reap's policy from `PSTORE_GC`, `PSTORE_GC_PERIOD_MS` and `PSTORE_GC_AGE_S`
/// (M18), refused by name as the fold's are -- and validated even when `off`.
fn gc_policy(get: &impl Fn(&str) -> Option<String>) -> Result<Option<GcPolicy>, ConfigError> {
    let on = match get("PSTORE_GC").as_deref() {
        None => true,
        Some("off") => false,
        Some(other) => {
            return Err(ConfigError::Fold(
                "PSTORE_GC",
                other.to_owned(),
                "unset, or off to leave reaping to the operator",
            ));
        }
    };
    let positive = |var: &'static str| -> Result<Option<u64>, ConfigError> {
        get(var)
            .map(|v| {
                v.parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or(ConfigError::Fold(var, v, "a positive integer"))
            })
            .transpose()
    };
    let d = GcPolicy::default();
    let policy = GcPolicy {
        period: positive("PSTORE_GC_PERIOD_MS")?.map_or(d.period, std::time::Duration::from_millis),
        age: reap_age(get)?,
    };
    Ok(on.then_some(policy))
}

/// `PSTORE_GC_AGE_S`, or the default an hour, read **whether or not the reap is on** (M39).
fn reap_age(get: &impl Fn(&str) -> Option<String>) -> Result<std::time::Duration, ConfigError> {
    get("PSTORE_GC_AGE_S")
        .map(|v| {
            v.parse::<u64>()
                .ok()
                .filter(|n| *n > 0)
                .map(std::time::Duration::from_secs)
                .ok_or(ConfigError::Fold(
                    "PSTORE_GC_AGE_S",
                    v,
                    "a positive integer",
                ))
        })
        .transpose()
        .map(|age| age.unwrap_or(GcPolicy::default().age))
}

/// Runs [`Api::fold_due`] every `policy.period` until `stop` resolves (M9i.1).
///
/// ⚠️ **Ticks never overlap**: a tick is awaited, then the loop sleeps. When `stop` resolves
/// mid-tick, no further tenant's fold starts, and the folds already in flight are awaited:
/// each commits by CAS or not at all, so stopping never leaves HEAD half-written.
pub async fn run_folds<S: BlobStore + 'static>(
    api: Arc<Api<S>>,
    policy: FoldPolicy,
    stop: impl std::future::Future<Output = ()> + Send,
) {
    let halt = std::sync::atomic::AtomicBool::new(false);
    tokio::pin!(stop);
    loop {
        let tick = api.fold_tick(&policy, &halt);
        tokio::pin!(tick);
        tokio::select! {
            () = &mut stop => {
                halt.store(true, std::sync::atomic::Ordering::SeqCst);
                tick.await;
                return;
            }
            _ = &mut tick => {}
        }
        tokio::select! {
            () = &mut stop => return,
            () = tokio::time::sleep(policy.period) => {}
        }
    }
}

/// Runs [`Api::reap_due`] every `policy.period` until `stop` resolves (M18), as [`run_folds`]
/// runs folds: ticks never overlap, and a stop mid-tick starts no further reap and awaits those
/// in flight, each of which commits by CAS or not at all.
pub async fn run_reaps<S: BlobStore + 'static>(
    api: Arc<Api<S>>,
    policy: GcPolicy,
    stop: impl std::future::Future<Output = ()> + Send,
) {
    let halt = std::sync::atomic::AtomicBool::new(false);
    tokio::pin!(stop);
    loop {
        let tick = api.reap_tick(&policy, &halt);
        tokio::pin!(tick);
        tokio::select! {
            () = &mut stop => {
                halt.store(true, std::sync::atomic::Ordering::SeqCst);
                tick.await;
                return;
            }
            _ = &mut tick => {}
        }
        tokio::select! {
            () = &mut stop => return,
            () = tokio::time::sleep(policy.period) => {}
        }
    }
}

/// Runs an eviction pass once a second until `stop` resolves (M26): whenever the registry has
/// a cap, whatever the fold, reap and replication loops are doing.
pub async fn run_evictions<S: BlobStore + 'static>(
    api: Arc<Api<S>>,
    stop: impl std::future::Future<Output = ()> + Send,
) {
    tokio::pin!(stop);
    loop {
        api.evict_tick().await;
        tokio::select! {
            () = &mut stop => return,
            () = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }
}

/// [`serve`], with the scheduled fold running beside it when `fold` is set (M9i.1), and the
/// scheduled reap when `gc` is (M18). One signal stops all three, and both loops have returned
/// before this does.
///
/// # Errors
/// If the server stops with an I/O error.
pub async fn serve_folding<S: BlobStore + 'static>(
    api: Arc<Api<S>>,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    fold: Option<FoldPolicy>,
    gc: Option<GcPolicy>,
    replicate: bool,
) -> std::io::Result<()> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let tx = Arc::new(tx);
    // M26: its own loop, never riding the fold or reap loops, either of which may be off.
    api.set_reaping(gc.is_some());
    let evictions = {
        let mut rx = rx.clone();
        tokio::spawn(run_evictions(Arc::clone(&api), async move {
            let _ = rx.wait_for(|stopped| *stopped).await;
        }))
    };
    let folds = fold.map(|policy| {
        let mut rx = rx.clone();
        tokio::spawn(run_folds(Arc::clone(&api), policy, async move {
            let _ = rx.wait_for(|stopped| *stopped).await;
        }))
    });
    let reaps = gc.map(|policy| {
        let mut rx = rx.clone();
        tokio::spawn(run_reaps(Arc::clone(&api), policy, async move {
            let _ = rx.wait_for(|stopped| *stopped).await;
        }))
    });
    // M22: the replication worker, under the same signal.
    let replication = replicate.then(|| {
        let mut rx = rx.clone();
        tokio::spawn(run_replication(Arc::clone(&api), async move {
            let _ = rx.wait_for(|stopped| *stopped).await;
        }))
    });
    let signal = Arc::clone(&tx);
    let served = serve(api, listener, async move {
        shutdown.await;
        let _ = signal.send(true);
    })
    .await;
    // Also when `serve` failed before its signal: the loop must not outlive the server.
    let _ = tx.send(true);
    if let Some(folds) = folds {
        let _ = folds.await;
    }
    if let Some(reaps) = reaps {
        let _ = reaps.await;
    }
    if let Some(r) = replication {
        let _ = r.await;
    }
    let _ = evictions.await;
    served
}

/// Serves dedicated MessagePack peers over mutually authenticated HTTP/2 and HTTP/3.
///
/// # Errors
/// Missing peer identity, mismatched listener authorities or a transport failure.
pub async fn serve_peers<S: BlobStore + 'static>(
    api: Arc<Api<S>>,
    tcp: tokio::net::TcpListener,
    udp: std::net::UdpSocket,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), std::io::Error> {
    let tls = api
        .auth
        .peer_tls()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "peer mTLS identity is required",
            )
        })?
        .0
        .clone();
    if tcp.local_addr()? != udp.local_addr()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "peer TCP and UDP listeners must share one authority",
        ));
    }
    let limits = api.peer_limits.clone();
    let handler: peer_http::Handler<peers::Command, peers::Outcome> =
        Arc::new(move |protocol, request| {
            let api = api.clone();
            Box::pin(async move {
                let phase = match &request.body {
                    peers::Command::Whole(_) => 0,
                    peers::Command::Open(_) => 1,
                    peers::Command::Scan(_) => 2,
                };
                let tenant = request
                    .tenant
                    .parse::<u128>()
                    .map(TenantId)
                    .map_err(|_| ApiError::bad_request("invalid peer tenant"));
                let result = match tenant {
                    Ok(tenant) => peers::execute(&api, tenant, request.body).await,
                    Err(error) => Err(error),
                };
                let (status, body) = match result {
                    Ok(body) => {
                        let transport = match protocol {
                            peer_http::Protocol::H2 => 0,
                            peer_http::Protocol::H3 => 1,
                        };
                        if let Some(counter) = api
                            .parts_completed
                            .get(transport)
                            .and_then(|phases| phases.get(phase))
                        {
                            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        (200, body)
                    }
                    Err(error) => (
                        error.status.as_u16(),
                        peers::Outcome::Error {
                            code: error.code.to_owned(),
                            message: error.message,
                            retryable: error.retryable,
                        },
                    ),
                };
                peer_wire::Reply {
                    version: peer_wire::VERSION,
                    id: request.id,
                    status,
                    body,
                }
            })
        });
    tokio::select! {
        result = peer_http::serve_with_limits(tcp, udp, tls, handler, limits) => result.map_err(std::io::Error::other),
        () = shutdown => Ok(()),
    }
}

/// Serves the public API until `shutdown` resolves.
///
/// # Errors
/// If the server stops with an I/O error.
pub async fn serve<S: BlobStore + 'static>(
    api: Arc<Api<S>>,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, api.router())
        .with_graceful_shutdown(shutdown)
        .await
}

/// The read cache `config` asks for over `store`, or `None` when it asks for none (M20).
///
/// ⚠️ **One GET at startup**, plus one conditional PUT and one GET the first time any node
/// meets the store: the store id. Per node, never per tenant. The disk tier itself never
/// fails to open: a directory it cannot use leaves the cache memory only.
///
/// # Errors
/// The store id could not be read or created.
pub async fn open_cache<S: BlobStore>(
    store: &Accounted<S>,
    config: &Config,
) -> Result<Option<Arc<CacheCore>>, pstore_blob::BlobError> {
    let Some(c) = &config.cache else {
        return Ok(None);
    };
    let identity = store_identity(store, &config.endpoint, &config.bucket).await?;
    let disk = pstore_cache::DiskConfig::new(&c.dir, config.lane.0, c.disk, identity);
    Ok(Some(Arc::new(CacheCore::open(c.ram, disk).await)))
}

/// Where a store keeps the id its read caches are validated by (M20).
const STORE_ID: &str = "_pstore/store-id";

/// The identity the read cache records for an S3 store (M20): its endpoint, its bucket, and
/// the random id the store itself holds at `_pstore/store-id`.
///
/// # Errors
/// A read or write of the id failed.
pub async fn store_identity<S: BlobStore>(
    store: &Accounted<S>,
    endpoint: &str,
    bucket: &str,
) -> Result<String, pstore_blob::BlobError> {
    // ⚠️ Outside every tenant: `_pstore/` is no tenant's prefix, which is four hex digits.
    let view = store.as_tenant(TenantId(0));
    let key = pstore_blob::Key::new(STORE_ID);
    let id = match view.get(&key).await {
        Ok(raw) => raw,
        Err(pstore_blob::BlobError::NotFound(_)) => {
            let mut bits = [0u8; 16];
            getrandom::fill(&mut bits)
                .map_err(|e| pstore_blob::BlobError::Other(format!("no randomness: {e}")))?;
            let fresh: String = bits.iter().map(|b| format!("{b:02x}")).collect();
            // Create-if-absent: two nodes starting at once agree on whichever landed.
            match view
                .put_conditional(
                    &key,
                    axum::body::Bytes::from(fresh.clone()),
                    pstore_blob::Precondition::NotExists,
                )
                .await
            {
                Ok(_) => axum::body::Bytes::from(fresh),
                Err(pstore_blob::CasError::Lost) => view.get(&key).await?,
                Err(e) => return Err(pstore_blob::BlobError::Other(e.to_string())),
            }
        }
        Err(e) => return Err(e),
    };
    Ok(format!(
        "s3 {endpoint} {bucket} {}",
        String::from_utf8_lossy(&id)
    ))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    /// M172: a filter literal above `i64` is a uint64, one within it an int, and what no type
    /// holds is refused as before.
    #[test]
    fn a_literal_beyond_i64_is_unsigned_and_the_rest_are_read_as_written() {
        use pstore_format::Value;
        assert_eq!(
            literal(&serde_json::json!(u64::MAX)),
            Ok(Value::Uint(u64::MAX))
        );
        assert_eq!(literal(&serde_json::json!(5)), Ok(Value::Int(5)));
        assert_eq!(literal(&serde_json::json!(-5)), Ok(Value::Int(-5)));
        assert!(literal(&serde_json::json!({"k": 1})).is_err());
        assert!(scalar(&serde_json::json!(u64::MAX)).is_err());
    }

    #[test]
    fn times_are_rfc3339_at_millisecond_precision() {
        assert_eq!(rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_ms(1_500), "1970-01-01T00:00:01.500Z");
        assert_eq!(rfc3339_ms(1_761_609_600_007), "2025-10-28T00:00:00.007Z");
    }

    /// Bodies the fast parse must answer exactly as the tree does (M120): valid ones of every
    /// shape, and each way a typed parse and a tree disagree.
    pub(crate) fn bodies() -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = [
            r#"{"documents": [{"id": "a", "vector": [1, 2.5, -3e2], "attributes": {"n": 1, "s": "x", "f": 1.5, "b": true, "l": [1, "y"]}}]}"#,
            r#"{"durability": "durable", "documents": [{"id": "a", "vector": "AACAPw==", "text": "hello"}], "deletes": ["b"]}"#,
            r#"{"durability": "async", "documents": []}"#,
            r#"{"durability": 5, "documents": []}"#,
            r#"{"durability": null}"#,
            r#"{"documents": [{"id": "a", "vector": [1]}], "documents": [{"id": "b", "vector": [2]}]}"#,
            r#"{"documents": [{"id": "a", "id": "b", "vector": [1]}]}"#,
            r#"{"documents": [{"id": "a", "vector": [1], "attributes": {"n": 1, "n": 2}}]}"#,
            r#"{"documents": [{"id": "a", "vector": [1], "attributes": {"n": 1, "\u006e": 2}}]}"#,
            r#"{"documents": [{"id": "a", "vector": [1e999]}]}"#,
            r#"{"documents": [{"id": "a", "vector": [-0, 18446744073709551616, 1e-50], "attributes": {"z": -0, "big": 18446744073709551616, "neg": -9223372036854775809}}]}"#,
            r#"{"documents": [{"id": "a", "vector": [1], "vectors": {"late": [[1, 2], "AACAPw=="]}}]}"#,
            r#"{"text": ["a", "b"], "top_k": 5}"#,
            r#"{"text": "a", "top_k": 5, "include_attributes": ["n", "s"]}"#,
            r#"{"text": "a", "top_k": 5, "include_attributes": true}"#,
            r#"{"multi": {"field": "late", "vectors": [[1, 2]]}, "top_k": 5}"#,
            r#"{"multi": {"field": "late", "vectors": [[1, 2]], "nope": 1}, "top_k": 5}"#,
            r#"{"vector": [-0, 1e-50], "top_k": 5, "as_of": null}"#,
            r#"{"vector": [1], "top_k": 18446744073709551616}"#,
            r#"{"vector": [1], "top_k": -1}"#,
            r#"{"documents": [], "x": 1e999}"#,
            r#"{"documents": [], "x": [1, {"y": -1e400}]}"#,
            r#"{"documents": [{"id": "a", "vector": [1, "a"]}]}"#,
            r#"{"documents": [{"id": "a", "vector": {}}]}"#,
            r#"{"documents": [{"vector": [1]}]}"#,
            r#"{"vector": [1, 0.5], "top_k": 3}"#,
            r#"{"vector": [1, 0.5], "top_k": 0}"#,
            r#"{"vector": [1, 0.5], "top_k": 3, "top_k": 4}"#,
            r#"{"text": "a b", "top_k": 10, "filters": ["n", "Gte", 3]}"#,
            r#"{"text": "a", "top_k": 10, "filters": ["And", [["n", "Lt", 3], ["s", "Eq", "x"]]]}"#,
            r#"{"queries": [{"vector": [1], "top_k": 1}]}"#,
            r#"{"queries": null}"#,
            r#"{"vector": [1], "top_k": 1, "nope": 1e999}"#,
            r#"{"protocol": 1, "phase": "open"}"#,
            r#"{"protocol": null, "phase": null}"#,
            r#"{"protocol": 2.0, "phase": 3}"#,
            r#"{"protocol": -1, "phase": "scan"}"#,
            r#"{"protocol": {"protocol": 1}, "phase": ["scan"]}"#,
            r#"{"\u0070rotocol": 1, "ph\u0061se": "open"}"#,
            r#"{"protocol": 18446744073709551615, "phase": "scan\u0021"}"#,
            r#"{"branch_from_namespace": "src"}"#,
            r#"{"branch_from_namespace": "src", "durability": "batched"}"#,
            r#"[]"#,
            r#"[{"durability": "async"}]"#,
            r#""x""#,
            r#"1"#,
            r#"null"#,
            r#"{}"#,
            r#"{} {}"#,
            r#""#,
        ]
        .iter()
        .map(|b| b.as_bytes().to_vec())
        .collect();
        // Invalid UTF-8, in a known field and in an unknown one.
        let mut bad = br#"{"documents": [{"id": "a"#.to_vec();
        bad.extend([0xff, 0xfe]);
        bad.extend(br#"", "vector": [1]}]}"#);
        out.push(bad);
        let mut bad = br#"{"documents": [], "x": "a"#.to_vec();
        bad.push(0xc0);
        bad.extend(br#""}"#);
        out.push(bad);
        // Every truncation of a valid body.
        let whole = out[0].clone();
        out.extend((0..whole.len()).map(|n| whole[..n].to_vec()));
        out
    }

    #[test]
    fn the_fast_parse_is_the_tree_parse() {
        // The tree's way, verbatim from before M120.
        // With the key count `branch_source` read from the tree before M122.
        fn tree_write(body: &[u8]) -> Result<(WriteRequest, usize), ApiError> {
            let raw: serde_json::Value = serde_json::from_slice(body)
                .map_err(|e| ApiError::bad_request(format!("malformed JSON: {e}")))?;
            let keys = raw.as_object().map_or(0, serde_json::Map::len);
            if let Some(d) = raw.get("durability").and_then(serde_json::Value::as_str)
                && !matches!(d, "batched" | "durable" | "applied")
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "unsupported_durability",
                    format!(
                        "durability {d:?} is not implemented; use \"batched\", \"durable\" or \"applied\""
                    ),
                ));
            }
            serde_json::from_value(raw)
                .map(|r| (r, keys))
                .map_err(|e| ApiError::bad_request(format!("malformed body: {e}")))
        }
        fn tree_query(body: &[u8]) -> Option<Result<QueryRequest, ApiError>> {
            let raw: serde_json::Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => return Some(Err(ApiError::bad_request(format!("malformed query: {e}")))),
            };
            // A multi-query is not the single path's.
            raw.get("queries").is_none().then(|| parse_query(raw))
        }
        fn either_query(body: &[u8]) -> Option<Result<QueryRequest, ApiError>> {
            match fast_query(body) {
                Some(req) => Some(checked_query(req)),
                None => tree_query(body),
            }
        }
        let shown = |r: Result<String, ApiError>| match r {
            Ok(s) => format!("ok {s}"),
            Err(e) => format!("{} {} {}", e.status, e.code, e.message),
        };
        let mut fast_hits = 0;
        for b in bodies() {
            let label = String::from_utf8_lossy(&b).into_owned();
            fast_hits += usize::from(body::typed::<WriteRequest>(&b).is_some())
                + usize::from(fast_query(&b).is_some());
            assert_eq!(
                shown(parse_write(&b).map(|r| format!("{r:?}"))),
                shown(tree_write(&b).map(|r| format!("{r:?}"))),
                "write: {label}"
            );
            assert_eq!(
                either_query(&b).map(|r| shown(r.map(|q| format!("{q:?}")))),
                tree_query(&b).map(|r| shown(r.map(|q| format!("{q:?}")))),
                "query: {label}"
            );
        }
        // The fast path is taken at all: a corpus that always fell back proves nothing.
        assert!(fast_hits >= 2, "the fast path parsed {fast_hits} bodies");
    }

    fn dense_tune(body: serde_json::Value) -> pstore_index::vec_index::Query {
        let req: QueryRequest = serde_json::from_value(body).unwrap();
        match prefetch(&req, req.top_k).unwrap().first() {
            Some(pstore_query::Prefetch::Dense { tune, .. }) => *tune,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_staleness_is_rounded_up_and_saturates() {
        use std::time::Duration;
        assert_eq!(millis(Duration::ZERO), 0);
        assert_eq!(millis(Duration::from_millis(7)), 7);
        assert_eq!(millis(Duration::from_micros(999_900)), 1000);
        assert_eq!(millis(Duration::from_nanos(1)), 1);
        assert_eq!(millis(Duration::from_secs(u64::MAX)), u64::MAX);
    }

    #[test]
    fn a_writes_footprint_counts_every_operation_once() {
        // M141: each part a distinct size, so a term dropped, doubled or combined the wrong
        // way changes the sum.
        let doc = Document::new("d00", vec![1.0, 0.5]);
        let patch = pstore_engine::Patch {
            id: "pp".to_owned(),
            set: [("ab".to_owned(), pstore_format::Value::Str("xyz".to_owned()))].into(),
            unset: vec!["u".to_owned()],
        };
        let by = pstore_engine::Patch {
            id: String::new(),
            set: [("kk".to_owned(), pstore_format::Value::Int(1))].into(),
            unset: Vec::new(),
        };
        let gone = pstore_query::Predicate::Absent("owner".to_owned());
        let even = pstore_query::Predicate::Cmp(
            "n".to_owned(),
            pstore_query::Op::Eq,
            pstore_format::Value::Int(2),
        );
        let (e1, e2) = (
            pstore_query::condition::encode(&gone).len() as u64,
            pstore_query::condition::encode(&even).len() as u64,
        );
        assert!(e1 > 0 && e2 > 0 && e1 != e2);
        let got = write_footprint(
            std::slice::from_ref(&doc),
            std::slice::from_ref(&patch),
            &["del".to_owned()],
            Some(&gone),
            Some(&(even, by)),
            7,
        );
        // The row, the patch (2 + 2 + 3 + 1), the tombstone (3 + 8), the two filters, the
        // patch by filter's patch (2 + 8), and the stamp on each of five operations.
        assert_eq!(got, doc.footprint() + 8 + 11 + e1 + e2 + 10 + 5 * 7);
        assert_eq!(write_footprint(&[], &[], &[], None, None, 7), 0);
    }

    #[test]
    fn exact_reaches_the_dense_leg() {
        // Code review of M9d: every server corpus is below the exact-scan threshold, so no
        // API test can tell a dropped `exact` from a forwarded one.
        assert!(dense_tune(serde_json::json!({"vector": [1.0], "exact": true})).exact);
        assert!(!dense_tune(serde_json::json!({"vector": [1.0]})).exact);
    }

    /// Read by the binary's startup log and by M128's tests. A unit test, so a mutant that
    /// forgets the configured limits fails in the first test binary cargo runs, rather than
    /// in the integration binary a CI mutation sweep can time out waiting for.
    #[test]
    fn an_api_reports_the_limits_it_was_given() {
        let limits = RequestLimits::builder()
            .active_requests(32)
            .build()
            .unwrap();
        let api = Api::with_limits(
            Accounted::new(pstore_blob::MemoryStore::new()),
            LaneId(1),
            None,
            Auth::unsecured(),
            limits,
        )
        .unwrap();
        assert_eq!(api.request_limits(), limits);
    }

    /// M170: the door's width checks, here so that a mutation breaking every write fails
    /// in this crate's unit tests rather than hanging its integration tests.
    #[test]
    fn widths_agree_takes_one_width_or_none_and_refuses_the_rest() {
        let req = |docs: serde_json::Value| -> WriteRequest {
            serde_json::from_value(serde_json::json!({ "documents": docs })).unwrap()
        };
        let agree = |docs| widths_agree(&req(docs)).is_ok();
        assert!(agree(serde_json::json!([])));
        assert!(agree(
            serde_json::json!([{"id": "a", "vector": [1.0]}, {"id": "b", "vector": [2.0]}])
        ));
        assert!(agree(
            serde_json::json!([{"id": "a"}, {"id": "b", "text": "x"}])
        ));
        assert!(!agree(serde_json::json!([{"id": "a", "vector": []}])));
        assert!(!agree(
            serde_json::json!([{"id": "a", "vector": [1.0]}, {"id": "b"}])
        ));
        assert!(!agree(
            serde_json::json!([{"id": "a", "vector": [1.0]}, {"id": "b", "vector": [1.0, 2.0]}])
        ));
    }
    /// M174: a batch in columns is the batch in rows, document for document, every null
    /// left out -- read by the same path, so a base64 vector and a declared type agree too.
    #[test]
    fn columns_normalize_to_the_rows_documents() {
        use base64::Engine as _;
        let b64 = |v: &[f32]| {
            let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        let schema = serde_json::json!({"owner": "uuid", "at": "datetime"});
        let u = "6f1e2d3c-4b5a-4968-8776-655443322110";
        let rows = serde_json::json!({"schema": schema, "documents": [
            {"id": "a", "vector": [1.0, 2.0], "vectors": {"late": [[1.0], [2.0]]},
             "text": "red fox",
             "attributes": {"n": 3, "f": 0.5, "b": true, "s": "x", "arr": [1, 2],
                            "owner": u, "at": "2026-10-29T01:02:03Z"}},
            {"id": "b", "vector": b64(&[3.0, 4.0]),
             "attributes": {"n": null, "f": 1.5, "s": "y", "owner": null}},
        ]});
        let columns = serde_json::json!({"schema": schema, "upsert_columns": {
            "id": ["a", "b"],
            "vector": [b64(&[1.0, 2.0]), [3.0, 4.0]],
            "vectors": [{"late": [[1.0], [2.0]]}, null],
            "text": ["red fox", null],
            "n": [3, null], "f": [0.5, 1.5], "b": [true, null], "s": ["x", "y"],
            "arr": [[1, 2], null], "owner": [u, null], "at": ["2026-10-29T01:02:03Z", null],
        }});
        let docs = |body: serde_json::Value| {
            let mut req: WriteRequest = serde_json::from_value(body).unwrap();
            upserts_of(&mut req).unwrap();
            let dated = declared_types(&req).unwrap();
            req.documents
                .into_iter()
                .map(|d| to_document(d, &dated).unwrap())
                .collect::<Vec<_>>()
        };
        let (rows, columns) = (docs(rows), docs(columns));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows, columns);
        let b = &rows[1];
        assert!(!b.attrs.contains_key("n") && !b.attrs.contains_key("owner"));
        assert!(b.attrs.contains_key("f"));
        assert_eq!(rows[0].attrs.len(), 8, "every attribute and the text field");
    }
}
