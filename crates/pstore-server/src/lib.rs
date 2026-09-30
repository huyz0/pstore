//! The HTTP surface — **the first door onto the engine** (M7c).
//!
//! `11-design/api-design.md` states the four principles this implements: the index is the
//! noun, every tradeoff is a client parameter, every response is honest about what it cost
//! and how fresh it is, and batches are the natural unit.
//!
//! ⚠️ **What this is not**, stated here because the alternative is implying otherwise
//! everywhere: there is **no authentication and no quota**. A tenant is whatever the request
//! header says it is. `pstore-meter` exists and is not wired. This server is not deployable
//! to anyone, and M7c's spec says so in one place rather than pretending.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use pstore_blob::{Accounted, BlobStore, OpClass, TenantView};
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

mod session;
mod types;
pub use types::{
    Cost, Duties, Duty, ErrorBody, FoldResponse, GcResponse, IndexList, IndexSummary, ListParams,
    MultiQueryMeta, MultiQueryResponse, QueryMeta, QueryRequest, QueryResponse, ResultRow, Schema,
    TextIn, WriteRequest, WriteResponse,
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
        instead: "terminate TLS in a reverse proxy in front of this process; the server \
                  speaks HTTP/1.1 in the clear and will not be given a certificate loader",
    },
    Duty {
        id: "auth",
        instead: "authenticate and authorize before the request reaches this process; the \
                  tenant is whatever the X-Pstore-Tenant header says it is",
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

/// One process's serving state: the store, the lane it writes on, and one engine per tenant.
///
/// ⚠️ **One `Engine` per tenant, held forever.** The engine owns the memtable that makes a
/// write visible before it is folded, so it cannot be rebuilt per request — and evicting one
/// with unflushed rows would discard acknowledged writes. The eviction policy is therefore a
/// decision about the durability contract, and M7c deliberately does not make it: the
/// registry is unbounded and the spec says so.
#[derive(Debug)]
pub struct Api<S> {
    store: Accounted<S>,
    lane: LaneId,
    engines: tokio::sync::Mutex<HashMap<TenantId, Arc<Engine<TenantView<S>>>>>,
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
            store,
            lane,
            engines: tokio::sync::Mutex::new(HashMap::new()),
            http: Mutex::new(Http::default()),
            backoff: Mutex::new(HashMap::new()),
            requested: Mutex::new(std::collections::HashSet::new()),
            reap_backoff: Mutex::new(HashMap::new()),
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
        let engines: Vec<(TenantId, Arc<Engine<TenantView<S>>>)> = self
            .engines
            .lock()
            .await
            .iter()
            .map(|(t, e)| (*t, Arc::clone(e)))
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
        let engines: Vec<(TenantId, Arc<Engine<TenantView<S>>>)> = self
            .engines
            .lock()
            .await
            .iter()
            .map(|(t, e)| (*t, Arc::clone(e)))
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
                Some((tenant, requested, engine.fold_committed().await))
            })
            .buffer_unordered(FOLD_CONCURRENCY)
            .filter_map(std::future::ready)
            .collect()
            .await;
        let mut backoff = self
            .backoff
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (tenant, requested, outcome) in results {
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
        Router::new()
            .route("/v1/indexes", get(list_indexes::<S>))
            .route(
                "/v1/indexes/{index}",
                get(index_summary::<S>).delete(delete_index::<S>),
            )
            .route("/v1/indexes/{index}/documents", put(write_documents::<S>))
            .route("/v1/indexes/{index}/query", post(query_index::<S>))
            .route(
                "/v1/indexes/{index}/schema",
                axum::routing::patch(patch_schema),
            )
            .route("/v1/admin/fold", post(fold_tenant::<S>))
            .route("/v1/admin/gc", post(gc_tenant::<S>))
            .route("/v1/admin/duties", get(duties))
            .route("/metrics", get(metrics::<S>))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&self),
                count_request::<S>,
            ))
            .with_state(self)
    }

    /// The engine for `tenant`, built on first use.
    async fn engine(&self, tenant: TenantId) -> Arc<Engine<TenantView<S>>> {
        let mut engines = self.engines.lock().await;
        Arc::clone(engines.entry(tenant).or_insert_with(|| {
            Arc::new(Engine::new(
                Arc::new(self.store.as_tenant(tenant)),
                tenant,
                self.lane,
            ))
        }))
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
            EngineError::DimensionMismatch { .. } => {
                Self::new(StatusCode::BAD_REQUEST, "schema_conflict", e.to_string())
            }
            EngineError::Unmeasurable(_) => Self::bad_request(e.to_string()),
            // M16: the client asked for a branch the engine will not make.
            EngineError::Refused(_) => Self::bad_request(e.to_string()),
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

/// A write's patches (M13), from `patch_rows` and `patch_columns`, in that order. Values are
/// converted as a document's attributes are; `null` removes an attribute.
fn patches_of(
    req: &WriteRequest,
    dated: &std::collections::BTreeSet<String>,
) -> Result<Vec<pstore_engine::Patch>, ApiError> {
    let mut rows: Vec<(String, serde_json::Map<String, serde_json::Value>)> = Vec::new();
    for raw in &req.patch_rows {
        let o = raw
            .as_object()
            .ok_or_else(|| ApiError::bad_request("a patch is {\"id\", \"attributes\"}"))?;
        let id = o
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ApiError::bad_request("a patch needs a string id"))?;
        if let Some(k) = o.keys().find(|k| *k != "id" && *k != "attributes") {
            return Err(ApiError::bad_request(if k == "vector" {
                format!("patch of {id}: a patch carries no vector; upsert the row to change it")
            } else {
                format!("patch of {id}: unknown field {k}")
            }));
        }
        let attrs = match o.get("attributes") {
            None => serde_json::Map::new(),
            Some(serde_json::Value::Object(m)) => m.clone(),
            Some(_) => {
                return Err(ApiError::bad_request(format!(
                    "patch of {id}: attributes is an object"
                )));
            }
        };
        rows.push((id.to_owned(), attrs));
    }
    if let Some(cols) = &req.patch_columns {
        let bad = |why: &str| ApiError::bad_request(format!("patch_columns: {why}"));
        let cols = cols
            .as_object()
            .ok_or_else(|| bad("an object of columns"))?;
        let ids = cols
            .get("id")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| bad("an `id` column is required"))?;
        let mut columns = Vec::new();
        for (name, col) in cols.iter().filter(|(n, _)| *n != "id") {
            if name == "vector" {
                return Err(bad(
                    "a patch carries no vector; upsert the rows to change them",
                ));
            }
            let col = col
                .as_array()
                .filter(|c| c.len() == ids.len())
                .ok_or_else(|| bad("every column must have one value per id"))?;
            columns.push((name, col));
        }
        for (i, id) in ids.iter().enumerate() {
            let id = id.as_str().ok_or_else(|| bad("ids are strings"))?;
            let attrs = columns
                .iter()
                .filter_map(|(name, col)| col.get(i).map(|v| ((*name).clone(), v.clone())))
                .collect();
            rows.push((id.to_owned(), attrs));
        }
    }
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
    id: String,
    who: &str,
    attrs: serde_json::Map<String, serde_json::Value>,
    dated: &std::collections::BTreeSet<String>,
) -> Result<pstore_engine::Patch, ApiError> {
    let unset: Vec<String> = attrs
        .iter()
        .filter(|(_, v)| v.is_null())
        .map(|(k, _)| k.clone())
        .collect();
    let set: serde_json::Map<String, serde_json::Value> =
        attrs.into_iter().filter(|(_, v)| !v.is_null()).collect();
    // Converted as a document's attributes are: every refusal of one is a patch's too.
    let as_doc: types::DocumentIn =
        serde_json::from_value(serde_json::json!({"id": id, "vector": [], "attributes": set}))
            .map_err(|e| ApiError::bad_request(format!("{who}: {e}")))?;
    for name in &unset {
        if name.is_empty() || name.starts_with('$') || name == pstore_query::ID_ATTRIBUTE {
            return Err(ApiError::bad_request(format!(
                "{who}: attribute `{name}` is reserved"
            )));
        }
    }
    let doc = to_document(&as_doc, dated)?;
    Ok(pstore_engine::Patch {
        id,
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
    dated: &std::collections::BTreeSet<String>,
) -> Result<(pstore_query::Predicate, pstore_engine::Patch), ApiError> {
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
    // Refusals from the document conversion name no operation, so they are prefixed here.
    let patch = patch_of(String::new(), "patch_by_filter", attrs.clone(), dated).map_err(|e| {
        if e.message.starts_with("patch_by_filter") {
            e
        } else {
            ApiError::bad_request(format!("patch_by_filter: {}", e.message))
        }
    })?;
    Ok((filter, patch))
}

/// `PUT /v1/indexes/{index}/documents`.
async fn write_documents<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let tenant = tenant_of(&headers)?;
    let mut token = session_of(&headers, tenant)?;
    // ⚠️ Parsed here rather than by the `Json` extractor, because the extractor's own
    // rejection is a body this API did not design -- and an unknown `durability` must come
    // back as `unsupported_durability` rather than as a serde message about an enum variant.
    let req: WriteRequest = parse_write(&body)?;
    if let Some(src) = branch_source(&req, &body)? {
        return branch_index(&api, tenant, token, src, &index).await;
    }
    let dated = declared_datetimes(&req)?;
    let declared = pstore_engine::Declared {
        fts: declared_fts(&req)?,
        trigram: declared_trigram(&req)?,
    };
    let patches = patches_of(&req, &dated)?;
    let delete_by = req
        .delete_by_filter
        .as_ref()
        .map(|v| write_filter("delete_by_filter", v))
        .transpose()?;
    let patch_by = req
        .patch_by_filter
        .as_ref()
        .map(|v| patch_by_filter_of(v, &dated))
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
    let condition = |name: &str, v: &Option<serde_json::Value>, any: bool| {
        v.as_ref()
            .map(|v| {
                if any {
                    // A condition travels to the fold encoded; one the fold could not decode
                    // would admit nothing there, so it is refused here (code review, M13.1).
                    write_filter(name, v)
                } else {
                    Err(ApiError::bad_request(format!(
                        "{name} with no operations of its kind to apply to"
                    )))
                }
            })
            .transpose()
    };
    let upsert_if = condition(
        "upsert_condition",
        &req.upsert_condition,
        !req.documents.is_empty(),
    )?;
    let patch_if = condition("patch_condition", &req.patch_condition, !patches.is_empty())?;
    let delete_if = condition(
        "delete_condition",
        &req.delete_condition,
        !req.deletes.is_empty(),
    )?;
    // ⚠️ **Deferred operations are durable only** (M13, spec review M4): a batched one would sit
    // where no fold reads it and no probe finds it, so a strong read here could miss it.
    let deferred = !patches.is_empty()
        || upsert_if.is_some()
        || delete_if.is_some()
        || delete_by.is_some()
        || patch_by.is_some();
    if deferred && !matches!(req.durability, types::Durability::Durable) {
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
    // ⚠️ **Refused at the door**, which is where `Engine::write` already refuses a document
    // the segment layout cannot store. A zero-dimension vector is storable and meaningless,
    // and a batch whose vectors disagree makes the *index's* dimension depend on which
    // document happened to be first. `api-design.md`: all vector dimensions in a field must
    // match — so the request that breaks that rule is the one that gets the error, rather
    // than the query that meets the consequence later.
    let dims = req.documents.first().map_or(0, |d| d.vector.len());
    if dims == 0 && !req.documents.is_empty() {
        return Err(ApiError::bad_request("a document with no vector"));
    }
    if let Some(odd) = req.documents.iter().find(|d| d.vector.len() != dims) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "schema_conflict",
            format!(
                "document {} has {} dimensions and the batch has {dims}",
                odd.id,
                odd.vector.len()
            ),
        ));
    }
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);

    // ⚠️ Every document converted BEFORE the engine sees any: a refusal must leave nothing
    // buffered, not the half of the batch that came before the bad attribute.
    let docs: Vec<Document> = req
        .documents
        .iter()
        .map(|d| to_document(d, &dated))
        .collect::<Result<_, _>>()?;
    let written = docs.len();
    if !docs.is_empty() {
        engine
            .write_declared(&index, docs, metric, upsert_if.as_ref(), &declared)
            .await?;
    }
    // Then the patches, then the deletes (M13): a later operation on an id wins.
    let patched = patches.len();
    if !patches.is_empty() {
        engine.patch(&index, patches, patch_if.as_ref()).await?;
    }
    // After the documents: a request that writes and deletes an id deletes it.
    let deleted = req.deletes.len();
    match &delete_if {
        Some(c) => engine.delete_if(&index, req.deletes, c).await?,
        None => engine.delete(&index, req.deletes).await?,
    }
    // Then the by-filter operations (M13.2), each one deferred operation: a patch, then a
    // delete, so a request that does both deletes what it patched into matching.
    if let Some((filter, patch)) = patch_by {
        engine.patch_by_filter(&index, &filter, patch).await?;
    }
    if let Some(filter) = &delete_by {
        engine.delete_by_filter(&index, filter).await?;
    }
    let durable = matches!(req.durability, types::Durability::Durable);
    if durable {
        engine.flush().await?;
        // ⚠️ **The engine's next sequence, not this flush's own** (M11.1, spec review B1): a
        // concurrent request's flush may have written these rows already, and this one then
        // flushed nothing. Every bundle the engine wrote is below its next sequence.
        if let Some(next) = engine.next_seq() {
            token.wrote(engine.lane().0, next);
        }
    }
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
        }),
        &session,
    ))
}

/// The index a write asks to branch from (M16), if it asks -- refused unless the request says
/// nothing else, since a branch beside a batch, a schema or a durability would have to mean an
/// order or a promise the branch does not make.
fn branch_source<'a>(req: &'a WriteRequest, body: &[u8]) -> Result<Option<&'a str>, ApiError> {
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
    // tell a stated one from an absent one.
    let raw: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request(format!("malformed JSON: {e}")))?;
    if raw.as_object().is_none_or(|o| o.len() != 1) {
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
    let raw: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("malformed query: {e}")))?;
    // ⚠️ Every query is planned -- every refusal found -- before any runs (M9g): one refused
    // sub-query refuses the request, and nothing partial is returned.
    let Some(subs) = raw.get("queries") else {
        let req = parse_query(raw)?;
        let plan = plan(&req)?;
        let engine = api.engine(tenant).await;
        let before = api.spend(tenant);
        let got = api
            .requesting(tenant, run(&engine, &index, &req, plan, &token).await)
            .await?;
        let session = got.token.encode();
        return Ok(with_session(
            axum::Json(QueryResponse {
                results: got.results,
                aggregations: got.aggregations,
                aggregation_groups: got.aggregation_groups,
                meta: QueryMeta {
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
    };
    let reqs = multi_query(&raw, subs)?;
    let plans = reqs.iter().map(plan).collect::<Result<Vec<_>, _>>()?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    // Concurrently: a multi-query costs its queries' requests at the deepest one's depth.
    let got = futures_util::future::try_join_all(
        reqs.iter()
            .zip(plans)
            .map(|(req, plan)| run(&engine, &index, req, plan, &token)),
    )
    .await;
    let got = api.requesting(tenant, got).await?;
    // From the sub-queries' own tokens, never the request's: each already carries it, less
    // the entries its HEAD covers, and merging the request's back would restore them.
    let mut merged = got
        .first()
        .map_or_else(|| token.clone(), |g| g.token.clone());
    for g in got.iter().skip(1) {
        merged.merge(&g.token);
    }
    let session = merged.encode();
    Ok(with_session(
        axum::Json(MultiQueryResponse {
            meta: MultiQueryMeta {
                epochs: got.iter().map(|g| g.epoch).collect(),
                unfolded_hits: got.iter().map(|g| g.unfolded_hits).collect(),
                cost: api.spend(tenant).since(before),
                consistencies: got.iter().map(|g| g.consistency).collect(),
                session: session.clone(),
                staleness_ms: got.iter().map(|g| g.staleness_ms).collect(),
            },
            results: got.into_iter().map(|g| g.results).collect(),
        }),
        &session,
    ))
}

/// The most queries one request may carry (M9g), turbopuffer's bound.
const MAX_QUERIES: usize = 16;

/// A query body, or why it is not one.
fn parse_query(raw: serde_json::Value) -> Result<QueryRequest, ApiError> {
    let req: QueryRequest = serde_json::from_value(raw)
        .map_err(|e| ApiError::bad_request(format!("malformed query: {e}")))?;
    if req.top_k == 0 {
        return Err(ApiError::bad_request("top_k must be greater than zero"));
    }
    Ok(req)
}

/// `{"queries": [...]}`'s sub-queries (M9g): 1 to 16, alone in the body, none nested.
fn multi_query(
    raw: &serde_json::Value,
    subs: &serde_json::Value,
) -> Result<Vec<QueryRequest>, ApiError> {
    if raw.as_object().is_some_and(|o| o.len() > 1) {
        return Err(ApiError::bad_request(
            "queries is the whole request: each query carries its own fields",
        ));
    }
    let subs = subs
        .as_array()
        .filter(|a| (1..=MAX_QUERIES).contains(&a.len()))
        .ok_or_else(|| {
            ApiError::bad_request(format!("queries is an array of 1 to {MAX_QUERIES} queries"))
        })?;
    subs.iter()
        .map(|q| {
            if q.get("queries").is_some() {
                return Err(ApiError::bad_request("a query inside queries may not nest"));
            }
            // A multi-query's results are lists of rows; an aggregation's are not (M12).
            if q.get("aggregate_by").is_some() {
                return Err(ApiError::bad_request(
                    "aggregate_by inside a multi-query is not offered",
                ));
            }
            parse_query(q.clone())
        })
        .collect()
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
        ("text", req.text.is_some()),
        ("rank_by", req.rank_by.is_some()),
        ("offset", req.offset.is_some()),
        ("fusion", req.fusion.is_some()),
        ("include_attributes", req.include_attributes.is_some()),
        ("exclude_attributes", req.exclude_attributes.is_some()),
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
        pstore_query::Total::Float(f) => serde_json::Value::from(f),
        pstore_query::Total::Null => serde_json::Value::Null,
    }
}

/// What a query will run, decided -- and refused -- before it runs.
enum Plan {
    /// Counts and sums, optionally grouped (M12).
    Aggregate(pstore_query::AggregateSpec, Option<pstore_query::Predicate>),
    /// A `rank_by` order (M9e).
    Ordered(pstore_query::OrderBy, Option<pstore_query::Predicate>),
    /// A relevance ranking: its legs, filter and fusion.
    Ranked(
        Vec<pstore_query::Prefetch>,
        Option<pstore_query::Predicate>,
        pstore_query::Fusion,
    ),
}

fn plan(req: &QueryRequest) -> Result<Plan, ApiError> {
    level(req)?;
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
    let legs = prefetch(req)?;
    let fusion = fusion(req, legs.len())?;
    Ok(Plan::Ranked(legs, filter, fusion))
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
    let (legs, filter, fusion) = match plan {
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
            let got = engine
                .ordered_as(
                    index,
                    &by,
                    filter.as_ref(),
                    req.offset.unwrap_or(0),
                    req.top_k,
                    req.as_of.map(pstore_types::Epoch),
                    consistency,
                )
                .await?;
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
                        attributes: selected(req, &d.attrs),
                        id: d.id,
                        score: None,
                        dist: None,
                    })
                    .collect(),
                epoch: got.epoch.0,
                unfolded_hits: got.unfolded,
                consistency: level_name(level),
            });
        }
        Plan::Ranked(legs, filter, fusion) => (legs, filter, fusion),
    };
    let answer = match req.as_of {
        Some(e) => {
            engine
                .query_as_of_filtered(
                    index,
                    pstore_types::Epoch(e),
                    &legs,
                    filter.as_ref(),
                    fusion,
                    req.top_k,
                )
                .await?
        }
        None => {
            engine
                .query_filtered_as(
                    index,
                    &legs,
                    filter.as_ref(),
                    fusion,
                    req.top_k,
                    consistency,
                )
                .await?
        }
    };
    let token = checked(answer.epoch.0, &answer.watermarks)?;
    if answer.segments.is_empty() && answer.unfolded.is_empty() {
        return Err(missing());
    }
    // ⚠️ `Answer` is explicit that a hit at `unfolded_at` indexes the unfolded rows rather
    // than HEAD's segments -- M5f's finding, where keying on the row alone merged two
    // unrelated documents into one.
    let unfolded_hits = answer
        .hits
        .iter()
        .filter(|h| h.segment == answer.unfolded_at)
        .count();
    Ok(Answered {
        results: engine
            .resolve_rows(&answer)
            .into_iter()
            .map(|(id, score, attrs, dist)| ResultRow {
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
                    "{} weights for {legs} legs; one per leg, the dense leg first",
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
        "sum" | "max" if req.vector.is_some() => Err(bad(&format!(
            "{kind} combines text legs' scores; a dense leg's can be negative"
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
    headers: HeaderMap,
) -> Result<axum::Json<IndexSummary>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    let (served, stats) = engine.index_stats_at(&index).await?;
    let unfolded = engine.pending_indexes().await.iter().any(|i| i == &index);
    if stats.is_none() && !unfolded {
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
        updated_epoch: None,
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
            regex: sc.trigram,
        }),
        rejected_rows: s.rejected_rows,
        updated_epoch: s.updated_epoch.map(|e| e.0),
        approx_row_count: s.documents,
        cost: api.spend(tenant).since(before),
    }))
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
    let mut names = engine.indexes().await?;
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

/// A write body, with the durability level refused by name rather than by serde's message.
fn parse_write(body: &[u8]) -> Result<WriteRequest, ApiError> {
    let raw: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request(format!("malformed JSON: {e}")))?;
    if let Some(d) = raw.get("durability").and_then(serde_json::Value::as_str)
        && !matches!(d, "batched" | "durable")
    {
        // ⚠️ **Refused, never downgraded.** Accepting `async` and treating it as `batched`
        // is a promise about durability that the system does not keep, which is the one
        // class of lie a storage system may never tell.
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "unsupported_durability",
            format!("durability {d:?} is not implemented; use \"batched\" or \"durable\""),
        ));
    }
    serde_json::from_value(raw).map_err(|e| ApiError::bad_request(format!("malformed body: {e}")))
}

/// The most a `rank_by` query may skip and return together (M9e): its rows are held in memory.
const MAX_ORDERED: usize = 10_000;

/// The order a query asks for, if it asks for one -- or why it cannot mean one (M9e).
///
/// ⚠️ **Refused, never combined.** An ordered answer and a relevance answer are different
/// questions, so `rank_by` beside `vector`, `text`, `field` or `exact: true` is a request that
/// means nothing -- and answering one of them would silently drop the other.
fn order_by(req: &QueryRequest) -> Result<Option<pstore_query::OrderBy>, ApiError> {
    let Some(raw) = &req.rank_by else {
        if req.offset.is_some() {
            return Err(ApiError::bad_request(
                "offset applies only to a rank_by order",
            ));
        }
        return Ok(None);
    };
    let by = match raw.as_array().map(Vec::as_slice) {
        Some(
            [
                serde_json::Value::String(attr),
                serde_json::Value::String(dir),
            ],
        ) => {
            let desc = match dir.as_str() {
                "asc" => false,
                "desc" => true,
                other => {
                    return Err(ApiError::bad_request(format!(
                        "rank_by direction {other:?} is not \"asc\" or \"desc\""
                    )));
                }
            };
            pstore_query::OrderBy {
                attr: attr.clone(),
                desc,
            }
        }
        _ => {
            return Err(ApiError::bad_request(
                "rank_by is [attribute, \"asc\" | \"desc\"]",
            ));
        }
    };
    if req.vector.is_some()
        || req.text.is_some()
        || req.field.is_some()
        || req.exact
        || req.fusion.is_some()
    {
        return Err(ApiError::bad_request(
            "rank_by orders by an attribute; it cannot be combined with vector, text, field, \
             exact or fusion",
        ));
    }
    if req.top_k.saturating_add(req.offset.unwrap_or(0)) > MAX_ORDERED {
        return Err(ApiError::bad_request(format!(
            "top_k + offset may be at most {MAX_ORDERED} for a rank_by order"
        )));
    }
    Ok(Some(by))
}

/// The legs a query asks for. A query with none is a request nobody meant to make.
fn prefetch(req: &QueryRequest) -> Result<Vec<pstore_query::Prefetch>, ApiError> {
    let mut legs = Vec::new();
    if let Some(v) = &req.vector {
        if v.is_empty() {
            return Err(ApiError::bad_request("an empty query vector"));
        }
        legs.push(pstore_query::Prefetch::Dense {
            field: req
                .field
                .clone()
                .unwrap_or_else(|| pstore_format::DEFAULT_FIELD.to_owned()),
            query: v.clone(),
            limit: req.top_k,
            tune: pstore_index::vec_index::Query {
                exact: req.exact,
                ..pstore_index::vec_index::Query::default()
            },
        });
    }
    let texts = req.text.as_ref().map(TextIn::queries).unwrap_or_default();
    if req.text.is_some() && !(1..pstore_query::MAX_LEGS).contains(&texts.len()) {
        return Err(ApiError::bad_request(format!(
            "text is a string or an array of 1 to {} strings",
            pstore_query::MAX_LEGS - 1
        )));
    }
    for t in texts {
        legs.push(pstore_query::Prefetch::Text {
            field: pstore_format::text::DEFAULT_TEXT_FIELD.to_owned(),
            query: t.to_owned(),
            limit: req.top_k,
        });
    }
    if legs.is_empty() {
        return Err(ApiError::bad_request(
            "a query must name a vector, text, or both",
        ));
    }
    Ok(legs)
}

/// The wire document, as the format's — or the reason it cannot be one.
///
/// ⚠️ **Refused, never coerced** (M9a). The format stores `i64`, strings, and since M9h.1
/// finite floats and bools; turning `1.5` into `1`, `true` into `"true"`, or `null` into an
/// absent attribute would store a value the client did not write, and nothing downstream
/// could tell.
/// The attribute names a write declares datetimes (M9h.3), or why its `schema` is refused.
///
/// ⚠️ A declaration that would do nothing is refused rather than ignored: another type, a
/// name no document uses, or the text field -- which is always a string -- would each
/// answer 200 and store something other than what the client meant.
fn declared_datetimes(req: &WriteRequest) -> Result<std::collections::BTreeSet<String>, ApiError> {
    let Some(schema) = &req.schema else {
        return Ok(std::collections::BTreeSet::new());
    };
    let bad = |why: String| ApiError::bad_request(format!("schema: {why}"));
    let schema = schema
        .as_object()
        .ok_or_else(|| bad("an object of attribute names to types".to_owned()))?;
    let mut out = std::collections::BTreeSet::new();
    for (name, ty) in schema {
        // An object naming `full_text_search` is a full-text declaration (M14), which
        // `declared_fts` reads; any other object is refused below, with a datetime's message.
        if ty.get("full_text_search").is_some() || ty.get("regex").is_some() {
            continue;
        }
        if ty.as_str() != Some("datetime") {
            return Err(bad(format!(
                "{name:?} is declared {ty}; datetime is the one type a write declares"
            )));
        }
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
        out.insert(name.clone());
    }
    Ok(out)
}

/// The full-text schema a write declares (M14): `{"<text field>": {"type": "string",
/// "full_text_search": true | {options}}}` in its `schema` map, or `None`.
fn declared_fts(req: &WriteRequest) -> Result<Option<pstore_format::text::FullText>, ApiError> {
    use pstore_format::text::{FullText, Language};
    let Some(schema) = req.schema.as_ref().and_then(serde_json::Value::as_object) else {
        return Ok(None);
    };
    let bad = |why: String| ApiError::bad_request(format!("schema: {why}"));
    let mut out = None;
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
        if decl.get("full_text_search").is_none() {
            continue;
        }
        if name != pstore_format::text::DEFAULT_TEXT_FIELD {
            return Err(bad(format!(
                "{name:?}: full_text_search is on the text field `{}` only",
                pstore_format::text::DEFAULT_TEXT_FIELD
            )));
        }
        if decl.get("type").is_some_and(|t| t != "string") {
            return Err(bad(format!("{name:?}: the text field's type is string")));
        }
        let mut fts = FullText::default();
        match decl.get("full_text_search") {
            Some(serde_json::Value::Bool(true)) => {}
            Some(serde_json::Value::Object(opts)) => {
                let flag = |v: &serde_json::Value, k: &str| {
                    v.as_bool()
                        .ok_or_else(|| bad(format!("full_text_search: {k} is true or false")))
                };
                let param = |v: &serde_json::Value, k: &str, hi: f64| {
                    v.as_f64()
                        .filter(|x| (0.0..=hi).contains(x))
                        // `+ 0.0` so `-0.0` is stored, and compared, as `0`.
                        .map(|x| x as f32 + 0.0)
                        .ok_or_else(|| {
                            bad(format!(
                                "full_text_search: {k} must be a number in [0, {hi}]"
                            ))
                        })
                };
                for (k, v) in opts {
                    let a = &mut fts.analyzer;
                    match k.as_str() {
                        "tokenizer" => {
                            if v != "word_v1" {
                                return Err(bad(format!(
                                    "full_text_search: tokenizer {v} is not word_v1, the one \
                                     tokenizer"
                                )));
                            }
                        }
                        "language" => {
                            a.language = v.as_str().and_then(Language::parse).ok_or_else(|| {
                                bad(format!(
                                    "full_text_search: language {v} is not one of {}",
                                    Language::ALL.map(Language::name).join(", ")
                                ))
                            })?;
                        }
                        "stemming" => a.stemming = flag(v, k)?,
                        "remove_stopwords" => a.remove_stopwords = flag(v, k)?,
                        "case_sensitive" => a.case_sensitive = flag(v, k)?,
                        "ascii_folding" => a.ascii_folding = flag(v, k)?,
                        "k1" => fts.k1 = param(v, k, 3.0)?,
                        "b" => fts.b = param(v, k, 1.0)?,
                        _ => return Err(bad(format!("full_text_search: unknown option {k:?}"))),
                    }
                }
                if fts.analyzer.remove_stopwords && fts.analyzer.language != Language::English {
                    return Err(bad(
                        "full_text_search: remove_stopwords is English only".to_owned()
                    ));
                }
            }
            _ => {
                return Err(bad(
                    "full_text_search is true, or an object of options".to_owned()
                ));
            }
        }
        if req.documents.is_empty() {
            return Err(bad(
                "full_text_search is declared and the write has no documents".to_owned(),
            ));
        }
        out = Some(fts);
    }
    Ok(out)
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
    serde_json::json!({
        "tokenizer": "word_v1",
        "language": a.language.name(),
        "stemming": a.stemming,
        "remove_stopwords": a.remove_stopwords,
        "case_sensitive": a.case_sensitive,
        "ascii_folding": a.ascii_folding,
        "k1": f.k1,
        "b": f.b,
    })
}

fn to_document(
    d: &types::DocumentIn,
    dated: &std::collections::BTreeSet<String>,
) -> Result<Document, ApiError> {
    let text_field = pstore_format::text::DEFAULT_TEXT_FIELD;
    let mut doc = Document::new(d.id.clone(), d.vector.clone());
    let refuse = |name: &str, why: &str| {
        ApiError::bad_request(format!("document {}: attribute `{name}` {why}", d.id))
    };
    for (name, value) in &d.attributes {
        if name.is_empty() {
            return Err(refuse(name, "has an empty name"));
        }
        // `$` names are the engine's (M9d: `$metric` carries a row's metric to the fold), as
        // turbopuffer reserves them for `$dist`.
        if name.starts_with('$') {
            return Err(refuse(name, "is reserved: names beginning `$` are"));
        }
        // `id` in a filter always means the document id (M9b); an attribute of that name
        // would give `["id", "Eq", x]` two meanings.
        if name == pstore_query::ID_ATTRIBUTE {
            return Err(refuse(name, "is reserved: `id` is the document id"));
        }
        let v = match value {
            // M9h.3: a declared datetime is an RFC 3339 string, or an array of them.
            _ if dated.contains(name) => declared_datetime(value),
            // M9h.2: an array of scalars; an element that is not one is refused, never dropped.
            serde_json::Value::Array(items) => items
                .iter()
                .map(|i| scalar(i).map_err(|why| format!("holds an element that {why}")))
                .collect::<Result<Vec<_>, _>>()
                .map(pstore_format::Value::Array),
            _ => scalar(value),
        };
        let v = match v {
            Ok(v) => v,
            Err(why) => return Err(refuse(name, &why)),
        };
        if name == text_field && !matches!(v, pstore_format::Value::Str(_)) {
            return Err(refuse(name, "is the text field and must be a string"));
        }
        doc.attrs.insert(name.clone(), v);
    }
    if let Some(t) = &d.text {
        // `attributes.text` and top-level `text` are one field spelled twice; which one
        // wins would be a rule nobody wrote down.
        if doc.attrs.contains_key(text_field) {
            return Err(refuse(
                text_field,
                "is given twice: as `text` and in `attributes`",
            ));
        }
        doc.attrs
            .insert(text_field.to_owned(), pstore_format::Value::Str(t.clone()));
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
        [attr, op, value] => {
            let attr = attr
                .as_str()
                .ok_or_else(|| bad("the attribute is a string".to_owned()))?
                .to_owned();
            let op = op
                .as_str()
                .ok_or_else(|| bad("the operator is a string".to_owned()))?;
            let scalar = |x: &serde_json::Value| scalar(x).map_err(|why| bad(format!("{x} {why}")));
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
                    let text = value
                        .as_str()
                        .ok_or_else(|| bad(format!("{op} takes a string of text")))?;
                    let how = match op {
                        "ContainsAllTokens" => pstore_query::TokenOp::All,
                        "ContainsAnyToken" => pstore_query::TokenOp::Any,
                        _ => pstore_query::TokenOp::Sequence,
                    };
                    Ok(Predicate::Tokens(attr, how, text.to_owned(), None))
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
                    Ok(Predicate::Fuzzy(attr, v.to_owned(), k as u8))
                }
                other => Err(bad(format!("unknown operator {other}"))),
            }
        }
        _ => Err(bad(
            "expected [attr, op, value], [\"And\"|\"Or\", [..]] or [\"Not\", filter]".to_owned(),
        )),
    }
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
                Err("is an integer beyond i64; integers must fit i64".to_owned())
            }
        }
        _ => Err("is not an integer, a float, a string or a bool, the attribute types".to_owned()),
    }
}

/// A value under a name the write declared a datetime (M9h.3).
fn declared_datetime(value: &serde_json::Value) -> Result<pstore_format::Value, String> {
    let one = |x: &serde_json::Value| {
        x.as_str()
            .and_then(pstore_format::datetime::parse)
            .map(pstore_format::Value::DateTime)
            .ok_or_else(|| {
                format!(
                    "is declared a datetime and {x} is not one: RFC 3339, \
                     YYYY-MM-DDTHH:MM:SS[.ffffff] and Z or ±HH:MM, years 0000 to 9999 in UTC"
                )
            })
    };
    match value {
        serde_json::Value::Array(items) => items
            .iter()
            .map(one)
            .collect::<Result<Vec<_>, _>>()
            .map(pstore_format::Value::Array),
        _ => one(value),
    }
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
}

/// Where a server keeps its data.
///
/// ⚠️ **There is no `azure` and no `gcs`, and their absence is a decision.** Every segment
/// open is a suffix read (`Segment::open`), and C-14 records suffix ranges as absent on Azure
/// three ways — the client refuses them before building a request, Azurite answers `bytes=-1`
/// with a 500, and the REST API has no suffix form. A server pointed at Azure would start and
/// then fail every query. `fake-gcs-server` accepts `ifGenerationMatch` and ignores it, which
/// is the worst shape a precondition can have. Neither is offered, so neither can be reached
/// by an operator who has not read this comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// In-process, durable for exactly as long as the process. The default M7c shipped.
    #[default]
    Memory,
    /// S3 or an S3-compatible endpoint.
    S3,
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
        "PSTORE_BACKEND={0} is not a backend this build has: memory, s3. Azure and GCS are \
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
    /// A scheduled-fold variable that is not what it must be (M9i.1).
    #[error("{0}={1} is refused: {2}")]
    Fold(&'static str, String, &'static str),
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
        Ok(Self {
            bind: get("PSTORE_BIND").unwrap_or_else(|| "127.0.0.1:8080".to_owned()),
            lane: LaneId(lane),
            backend,
            profile,
            endpoint,
            bucket: get("PSTORE_BUCKET").unwrap_or_else(|| "pstore".to_owned()),
            credentials: get("PSTORE_ACCESS_KEY").zip(get("PSTORE_SECRET_KEY")),
            fold: fold_policy(&get)?,
            gc: gc_policy(&get)?,
        })
    }
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
        age: positive("PSTORE_GC_AGE_S")?.map_or(d.age, std::time::Duration::from_secs),
    };
    Ok(on.then_some(policy))
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
) -> std::io::Result<()> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let tx = Arc::new(tx);
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
    served
}

/// Serves until `shutdown` resolves.
///
/// ⚠️ Takes a bound listener rather than an address, so a caller — a test, or a supervisor
/// passing a socket down — decides what it is listening on. Returning on shutdown rather than
/// being killed is what makes "the process stops cleanly" a thing a test can assert.
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn dense_tune(body: serde_json::Value) -> pstore_index::vec_index::Query {
        let req: QueryRequest = serde_json::from_value(body).unwrap();
        match prefetch(&req).unwrap().first() {
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
    fn exact_reaches_the_dense_leg() {
        // Code review of M9d: every server corpus is below the exact-scan threshold, so no
        // API test can tell a dropped `exact` from a forwarded one.
        assert!(dense_tune(serde_json::json!({"vector": [1.0], "exact": true})).exact);
        assert!(!dense_tune(serde_json::json!({"vector": [1.0]})).exact);
    }
}
