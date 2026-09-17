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

mod types;
pub use types::{
    Cost, ErrorBody, FoldResponse, GcResponse, IndexList, IndexSummary, QueryMeta, QueryRequest,
    QueryResponse, ResultRow, Schema, WriteRequest, WriteResponse,
};

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
        }))
    }

    /// The routes. `api-design.md`'s surface, minus everything M7c defers.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/v1/indexes", get(list_indexes::<S>))
            .route("/v1/indexes/{index}", get(index_summary::<S>))
            .route("/v1/indexes/{index}/documents", put(write_documents::<S>))
            .route("/v1/indexes/{index}/query", post(query_index::<S>))
            .route(
                "/v1/indexes/{index}/schema",
                axum::routing::patch(patch_schema),
            )
            .route("/v1/admin/fold", post(fold_tenant::<S>))
            .route("/v1/admin/gc", post(gc_tenant::<S>))
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
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = self.code;
        let body = ErrorBody::new(self.code, self.message, self.retryable);
        let mut res = (self.status, axum::Json(body)).into_response();
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
    "internal",
];

/// The refusal code, carried out of `IntoResponse` so a layer can count it.
#[derive(Debug, Clone, Copy)]
struct RefusalCode(&'static str);

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

/// `PUT /v1/indexes/{index}/documents`.
async fn write_documents<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::Json<WriteResponse>, ApiError> {
    let tenant = tenant_of(&headers)?;
    // ⚠️ Parsed here rather than by the `Json` extractor, because the extractor's own
    // rejection is a body this API did not design -- and an unknown `durability` must come
    // back as `unsupported_durability` rather than as a serde message about an enum variant.
    let req: WriteRequest = parse_write(&body)?;
    if req.documents.is_empty() {
        return Err(ApiError::bad_request("a write with no documents"));
    }
    // ⚠️ **Refused at the door**, which is where `Engine::write` already refuses a document
    // the segment layout cannot store. A zero-dimension vector is storable and meaningless,
    // and a batch whose vectors disagree makes the *index's* dimension depend on which
    // document happened to be first. `api-design.md`: all vector dimensions in a field must
    // match — so the request that breaks that rule is the one that gets the error, rather
    // than the query that meets the consequence later.
    let dims = req.documents.first().map_or(0, |d| d.vector.len());
    if dims == 0 {
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

    let docs: Vec<Document> = req.documents.iter().map(to_document).collect();
    let written = docs.len();
    engine.write(&index, docs).await?;
    let durable = matches!(req.durability, types::Durability::Durable);
    if durable {
        engine.flush().await?;
    }
    Ok(axum::Json(WriteResponse {
        epoch: engine.epoch().0,
        documents_written: written,
        durable,
        cost: api.spend(tenant).since(before),
    }))
}

/// `POST /v1/indexes/{index}/query`.
async fn query_index<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::Json<QueryResponse>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let req: QueryRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("malformed query: {e}")))?;
    if req.top_k == 0 {
        return Err(ApiError::bad_request("top_k must be greater than zero"));
    }
    let legs = prefetch(&req)?;

    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    // ⚠️ **One read of HEAD for the whole request.** Existence is decided from the snapshot
    // the query already read -- asking first would double the cost of every query, and
    // resolving afterwards against a second read would race a fold. `Answer::segments` is
    // what carries it.
    // ⚠️ Time travel is the same query against a manifest reconstructed from the HEAD it
    // already reads -- no archive, no extra request -- and `meta.epoch` reports which epoch
    // was served, because a stale answer indistinguishable from a fresh one is worse than no
    // answer at all.
    let answer = match req.as_of {
        Some(epoch) => {
            engine
                .query_as_of(
                    &index,
                    pstore_types::Epoch(epoch),
                    &legs,
                    pstore_query::Fusion::Rrf { k: 60.0 },
                    req.top_k,
                )
                .await?
        }
        None => {
            engine
                .query(
                    &index,
                    &legs,
                    pstore_query::Fusion::Rrf { k: 60.0 },
                    req.top_k,
                )
                .await?
        }
    };
    if answer.segments.is_empty() && answer.unfolded.is_empty() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "index_not_found",
            format!("this tenant has no index {index}"),
        ));
    }

    // ⚠️ `Answer` is explicit that a hit at `unfolded_at` indexes the unfolded rows rather
    // than HEAD's segments — M5f's finding, where keying on the row alone merged two
    // unrelated documents into one. `Engine::resolve` is what knows that; the handler counts
    // the fresh ones so the caller can see which half of the answer it is looking at.
    let unfolded_hits = answer
        .hits
        .iter()
        .filter(|h| h.segment == answer.unfolded_at)
        .count();
    let results = engine
        .resolve(&answer)
        .into_iter()
        .map(|(id, score)| ResultRow { id, score })
        .collect();
    Ok(axum::Json(QueryResponse {
        results,
        meta: QueryMeta {
            epoch: req.as_of.unwrap_or_else(|| engine.epoch().0),
            unfolded_hits,
            cost: api.spend(tenant).since(before),
        },
    }))
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
    let stats = engine.index_stats(&index).await?;
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
        epoch: engine.epoch(),
        schema: None,
        rejected_rows: 0,
    });
    Ok(axum::Json(IndexSummary {
        index,
        segments: s.segments,
        documents: s.documents,
        epoch: s.epoch.0,
        unfolded,
        schema: s.schema.map(|sc| SchemaOut {
            dims: sc.dims,
            text_field: sc.text_field,
        }),
        rejected_rows: s.rejected_rows,
        cost: api.spend(tenant).since(before),
    }))
}

/// `GET /v1/indexes`.
async fn list_indexes<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    headers: HeaderMap,
) -> Result<axum::Json<IndexList>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    // ⚠️ **One read, never a LIST.** A LIST over the tenant's prefix is the obvious
    // implementation, is correct, and is priced like a PUT while returning at most 1000 keys.
    let mut names = engine.indexes().await?;
    names.extend(engine.pending_indexes().await);
    names.sort_unstable();
    names.dedup();
    Ok(axum::Json(IndexList {
        indexes: names,
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
            tune: pstore_index::vec_index::Query::default(),
        });
    }
    if let Some(t) = &req.text {
        legs.push(pstore_query::Prefetch::Text {
            field: pstore_format::text::DEFAULT_TEXT_FIELD.to_owned(),
            query: t.clone(),
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

/// The wire document, as the format's.
fn to_document(d: &types::DocumentIn) -> Document {
    let mut doc = Document::new(d.id.clone(), d.vector.clone());
    if let Some(t) = &d.text {
        doc.attrs.insert(
            pstore_format::text::DEFAULT_TEXT_FIELD.to_owned(),
            pstore_format::Value::Str(t.clone()),
        );
    }
    doc
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
    /// writing one tenant on the same lane both start at sequence zero and overwrite each
    /// other's bundles — acknowledged, durable writes, gone, with no error anywhere, because
    /// a bundle is an unconditional `put`. A default here would make that the *normal* way to
    /// run two servers. Nothing detects the collision today; the backlog names create-if-absent
    /// bundles as the fix and M2's density invariant as why it is not a one-line change.
    pub lane: LaneId,
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
        Ok(Self {
            bind: get("PSTORE_BIND").unwrap_or_else(|| "127.0.0.1:8080".to_owned()),
            lane: LaneId(lane),
        })
    }
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
