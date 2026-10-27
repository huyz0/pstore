//! Bounded public request ingress (M128): body bytes, active requests, and a read deadline.
//!
//! ⚠️ **Admission is process-local resource protection, and nothing more.** It confers no
//! ownership, lock or lease, and a `429` is only ever issued before dispatch: a request that
//! reached its handler follows the handler's own completion semantics. Nothing here replays
//! or retries a mutation.
//!
//! ⚠️ **Reserved before polling.** The maximum admitted body is reserved from the byte pool
//! before the first frame is read and held until the handler's response exists, so the pool
//! bounds the bodies this server collects -- not RSS, decoded JSON, or engine state, which
//! are separate resource classes.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use pstore_types::TenantId;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ApiError;

/// The one configuration version this release reads.
const VERSION: u64 = 1;
/// `PSTORE_REQUEST_LIMITS_CONFIG` is refused past this many bytes.
const MAX_CONFIG: usize = 4096;

/// Immutable, versioned public ingress limits, shared by every router of one `Api`.
///
/// Version 1's defaults are the ceiling: configuration can lower them, never raise them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestLimits {
    body_bytes: usize,
    active: usize,
    tenant_active: usize,
    reserved_body_bytes: usize,
    body_read_timeout: Duration,
}

impl Default for RequestLimits {
    /// Version 1: 2 MiB bodies, 64 active requests, 8 per tenant, a 128 MiB body pool and a
    /// 30 s body-read deadline.
    fn default() -> Self {
        Self {
            body_bytes: 2 << 20,
            active: 64,
            tenant_active: 8,
            reserved_body_bytes: 128 << 20,
            body_read_timeout: Duration::from_secs(30),
        }
    }
}

/// Why a set of request limits was refused. Every variant names the setting, never a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LimitsError {
    /// `PSTORE_REQUEST_LIMITS_CONFIG` is not version 1's strict JSON, or is too long.
    #[error("request limits configuration is malformed: {0}")]
    Malformed(String),
    /// A limit is zero.
    #[error("request limit {0} must be positive")]
    Zero(&'static str),
    /// A limit is above version 1's default, which configuration can only lower.
    #[error("request limit {0} is above its version 1 default")]
    AboveDefault(&'static str),
    /// More active requests per tenant than in all.
    #[error("request limits: tenant_active_requests exceeds active_requests")]
    TenantAboveGlobal,
    /// The body pool cannot hold one maximum body.
    #[error("request limits: reserved_body_bytes is smaller than body_bytes")]
    PoolBelowBody,
}

/// A checked way to lower [`RequestLimits`]; it starts at the defaults.
#[derive(Debug, Clone, Copy)]
pub struct RequestLimitsBuilder(RequestLimits);

impl RequestLimitsBuilder {
    /// The largest encoded body admitted, in bytes.
    #[must_use]
    pub fn body_bytes(mut self, n: usize) -> Self {
        self.0.body_bytes = n;
        self
    }

    /// Public requests active at once, across every tenant.
    #[must_use]
    pub fn active_requests(mut self, n: usize) -> Self {
        self.0.active = n;
        self
    }

    /// Public requests active at once for one tenant (or the private cohort).
    #[must_use]
    pub fn tenant_active_requests(mut self, n: usize) -> Self {
        self.0.tenant_active = n;
        self
    }

    /// Body bytes reserved at once, across every active request.
    #[must_use]
    pub fn reserved_body_bytes(mut self, n: usize) -> Self {
        self.0.reserved_body_bytes = n;
        self
    }

    /// How long a body may take to arrive.
    #[must_use]
    pub fn body_read_timeout(mut self, d: Duration) -> Self {
        self.0.body_read_timeout = d;
        self
    }

    /// The limits, or why they are unsafe.
    ///
    /// # Errors
    /// A zero value, a value above its default, a tenant capacity above the global one, or a
    /// pool smaller than one maximum body.
    pub fn build(self) -> Result<RequestLimits, LimitsError> {
        let l = self.0;
        let d = RequestLimits::default();
        for (name, got, ceiling) in [
            ("body_bytes", l.body_bytes, d.body_bytes),
            ("active_requests", l.active, d.active),
            ("tenant_active_requests", l.tenant_active, d.tenant_active),
            (
                "reserved_body_bytes",
                l.reserved_body_bytes,
                d.reserved_body_bytes,
            ),
        ] {
            if got == 0 {
                return Err(LimitsError::Zero(name));
            }
            if got > ceiling {
                return Err(LimitsError::AboveDefault(name));
            }
        }
        if l.body_read_timeout.is_zero() {
            return Err(LimitsError::Zero("body_read_timeout_ms"));
        }
        if l.body_read_timeout > d.body_read_timeout {
            return Err(LimitsError::AboveDefault("body_read_timeout_ms"));
        }
        if l.tenant_active > l.active {
            return Err(LimitsError::TenantAboveGlobal);
        }
        if l.reserved_body_bytes < l.body_bytes {
            return Err(LimitsError::PoolBelowBody);
        }
        Ok(l)
    }
}

/// `PSTORE_REQUEST_LIMITS_CONFIG`, version 1. Every field but `version` is optional.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u64,
    body_bytes: Option<u64>,
    active_requests: Option<u64>,
    tenant_active_requests: Option<u64>,
    reserved_body_bytes: Option<u64>,
    body_read_timeout_ms: Option<u64>,
}

impl RequestLimits {
    /// A builder that starts at the defaults.
    #[must_use]
    pub fn builder() -> RequestLimitsBuilder {
        RequestLimitsBuilder(Self::default())
    }

    /// Reads `PSTORE_REQUEST_LIMITS_CONFIG`; absent, the defaults.
    ///
    /// # Errors
    /// Malformed JSON, an unknown field or version, an integer that does not fit, or any
    /// refusal of [`RequestLimitsBuilder::build`].
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, LimitsError> {
        let Some(raw) = get("PSTORE_REQUEST_LIMITS_CONFIG") else {
            return Ok(Self::default());
        };
        if raw.len() > MAX_CONFIG {
            return Err(LimitsError::Malformed("too long".to_owned()));
        }
        let c: Config =
            serde_json::from_str(&raw).map_err(|e| LimitsError::Malformed(e.to_string()))?;
        if c.version != VERSION {
            return Err(LimitsError::Malformed(format!("version must be {VERSION}")));
        }
        let size = |name: &'static str, v: Option<u64>, default: usize| match v {
            None => Ok(default),
            Some(v) => usize::try_from(v).map_err(|_| LimitsError::AboveDefault(name)),
        };
        let d = Self::default();
        let timeout = c
            .body_read_timeout_ms
            .map_or(d.body_read_timeout, Duration::from_millis);
        Self::builder()
            .body_bytes(size("body_bytes", c.body_bytes, d.body_bytes)?)
            .active_requests(size("active_requests", c.active_requests, d.active)?)
            .tenant_active_requests(size(
                "tenant_active_requests",
                c.tenant_active_requests,
                d.tenant_active,
            )?)
            .reserved_body_bytes(size(
                "reserved_body_bytes",
                c.reserved_body_bytes,
                d.reserved_body_bytes,
            )?)
            .body_read_timeout(timeout)
            .build()
    }

    /// The largest encoded body admitted, in bytes.
    #[must_use]
    pub fn body_bytes(&self) -> usize {
        self.body_bytes
    }

    /// Public requests active at once.
    #[must_use]
    pub fn active_requests(&self) -> usize {
        self.active
    }

    /// Public requests active at once for one tenant.
    #[must_use]
    pub fn tenant_active_requests(&self) -> usize {
        self.tenant_active
    }

    /// Body bytes reserved at once.
    #[must_use]
    pub fn reserved_body_bytes(&self) -> usize {
        self.reserved_body_bytes
    }

    /// How long a body may take to arrive.
    #[must_use]
    pub fn body_read_timeout(&self) -> Duration {
        self.body_read_timeout
    }
}

/// Who a request is counted against.
///
/// ⚠️ **Never an identity the request invented.** A verified bearer's tenant, or a private
/// mode tenant header the handler would read anyway; a private request without one (an
/// administrative call) joins the one private cohort rather than minting a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Tenant(TenantId),
    Private,
}

/// What is admitted now. An idle tenant has no entry.
#[derive(Debug, Default)]
struct Counts {
    active: usize,
    bytes: usize,
    tenants: HashMap<Key, usize>,
}

/// One `Api`'s admission state, shared by every router it builds.
#[derive(Debug)]
pub(crate) struct Ingress {
    limits: RequestLimits,
    counts: Mutex<Counts>,
}

/// What admission holds right now, for tests and measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IngressCounts {
    /// Requests admitted and not yet answered.
    pub active: usize,
    /// Body bytes reserved for them.
    pub reserved_body_bytes: usize,
    /// Tenants (and the private cohort) with at least one of them.
    pub tenants: usize,
}

/// One admitted request's share. Dropping it, on any path, returns all of it.
struct Permit {
    ingress: Arc<Ingress>,
    key: Key,
    bytes: usize,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut c = self.ingress.lock();
        c.active = c.active.saturating_sub(1);
        c.bytes = c.bytes.saturating_sub(self.bytes);
        if let Some(n) = c.tenants.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                c.tenants.remove(&self.key);
            }
        }
    }
}

impl Ingress {
    pub(crate) fn new(limits: RequestLimits) -> Self {
        Self {
            limits,
            counts: Mutex::new(Counts::default()),
        }
    }

    pub(crate) fn limits(&self) -> &RequestLimits {
        &self.limits
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Counts> {
        self.counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn counts(&self) -> IngressCounts {
        let c = self.lock();
        IngressCounts {
            active: c.active,
            reserved_body_bytes: c.bytes,
            tenants: c.tenants.len(),
        }
    }

    /// A permit for `key`, or `None` at once when any capacity is spent. Never waits.
    fn admit(self: &Arc<Self>, key: Key) -> Option<Permit> {
        let bytes = self.limits.body_bytes;
        let mut c = self.lock();
        let mine = c.tenants.get(&key).copied().unwrap_or(0);
        if c.active >= self.limits.active
            || mine >= self.limits.tenant_active
            || c.bytes + bytes > self.limits.reserved_body_bytes
        {
            return None;
        }
        c.active += 1;
        c.bytes += bytes;
        *c.tenants.entry(key).or_default() += 1;
        drop(c);
        Some(Permit {
            ingress: Arc::clone(self),
            key,
            bytes,
        })
    }
}

fn too_large() -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "request_too_large",
        "the request body is larger than this server admits",
    )
}

fn overloaded() -> ApiError {
    ApiError {
        retry_after: Some(1),
        ..ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "overloaded",
            "this server is at its local request capacity; retry",
        )
    }
}

fn timed_out() -> ApiError {
    ApiError::new(
        StatusCode::REQUEST_TIMEOUT,
        "request_timeout",
        "the request body did not arrive before the read deadline",
    )
}

/// `true` when no `Content-Encoding` other than `identity` is declared.
fn identity_encoded(headers: &HeaderMap) -> bool {
    headers.get_all(header::CONTENT_ENCODING).iter().all(|v| {
        v.to_str().is_ok_and(|s| {
            s.split(',')
                .map(str::trim)
                .all(|c| c.is_empty() || c.eq_ignore_ascii_case("identity"))
        })
    })
}

/// `true` when some `Content-Length` already says the body is over `max`. A length is only
/// ever a reason to refuse early: collection counts the bytes that arrive.
fn declared_over(headers: &HeaderMap, max: usize) -> bool {
    headers.get_all(header::CONTENT_LENGTH).iter().any(|v| {
        v.to_str()
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .is_some_and(|n| n > max as u64)
    })
}

/// Who `req` is counted against. Authentication has already rewritten `x-pstore-tenant`
/// to a bearer's verified tenant (and refused one that claimed another), so the header is
/// the identity in both modes; headerless private requests share one cohort.
fn key_of(req: &Request) -> Key {
    req.headers()
        .get("x-pstore-tenant")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u128>().ok())
        .map_or(Key::Private, |t| Key::Tenant(TenantId(t)))
}

/// Reads `body` whole, refusing past `max` bytes or `timeout`. `Limited` counts the bytes
/// that arrive and refuses at the first chunk past `max`; one chunk is kept uncopied.
async fn collect(body: Body, max: usize, timeout: Duration) -> Result<bytes::Bytes, ApiError> {
    match tokio::time::timeout(timeout, Limited::new(body, max).collect()).await {
        Err(_) => Err(timed_out()),
        Ok(Ok(collected)) => Ok(collected.to_bytes()),
        Ok(Err(e)) if e.is::<LengthLimitError>() => Err(too_large()),
        Ok(Err(_)) => Err(ApiError::bad_request("the request body could not be read")),
    }
}

/// The ingress layer: after authentication and the route's scope, before the handler.
pub(crate) async fn admit(
    State(ingress): State<Arc<Ingress>>,
    req: Request,
    next: Next,
) -> Response {
    if !identity_encoded(req.headers()) {
        return ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_content_encoding",
            "request bodies must not be content-encoded; decompression is unsupported",
        )
        .into_response();
    }
    let limits = ingress.limits;
    if declared_over(req.headers(), limits.body_bytes) {
        return too_large().into_response();
    }
    let Some(permit) = ingress.admit(key_of(&req)) else {
        return overloaded().into_response();
    };
    let (mut parts, body) = req.into_parts();
    let bytes = match collect(body, limits.body_bytes, limits.body_read_timeout).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    parts
        .headers
        .insert(header::CONTENT_LENGTH, bytes.len().into());
    let res = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    drop(permit);
    res
}

#[cfg(test)]
mod tests;
