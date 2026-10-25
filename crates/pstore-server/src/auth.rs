//! Immutable local bearer policy. No credential lookup reaches the blob store.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::MethodRouter;
use base64::Engine as _;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use pstore_types::TenantId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::ApiError;

const MAX_TOKEN: usize = 8192;
const MAX_CONFIG: usize = 65536;

/// Authentication policy chosen once when constructing a serving process.
#[derive(Clone)]
pub struct Auth(Policy, Option<(crate::PeerTls, crate::PeerTransport)>);

#[derive(Clone)]
enum Policy {
    Closed,
    Unsecured,
    Bearer(Arc<Keyring>),
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            Policy::Closed => "Auth(Closed)",
            Policy::Unsecured => "Auth(Unsecured)",
            Policy::Bearer(_) => "Auth(Bearer)",
        })
    }
}

/// A redacted configuration or signing failure.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("authentication configuration or credential is invalid")]
pub struct AuthError;

struct Keyring {
    keys: BTreeMap<String, Key>,
    validation: Validation,
}

struct Key {
    verify: DecodingKey,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    issuer: String,
    audience: String,
    active_kid: String,
    keys: Vec<KeyConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyConfig {
    kid: String,
    secret: String,
}

/// Roles are independent; no role implicitly confers another role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Scope {
    Read,
    Write,
    List,
    Admin,
    Internal,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Source {
    tenant: String,
    store: String,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize, Serialize)]
struct Claims {
    iss: String,
    aud: Audience,
    exp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    nbf: Option<u64>,
    tenant: String,
    scopes: Vec<Scope>,
    #[serde(default)]
    sources: Vec<Source>,
}

/// Verified authority, kept in request extensions rather than client headers.
#[derive(Clone)]
pub(crate) enum Principal {
    Unsecured,
    Verified {
        tenant: TenantId,
        scopes: Vec<Scope>,
        sources: Vec<(TenantId, String)>,
    },
}

fn decimal(value: &str) -> Option<TenantId> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().map(TenantId)
}

fn now() -> Result<u64, AuthError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| AuthError)
}

fn active_at(exp: u64, nbf: Option<u64>, now: u64) -> bool {
    now < exp && nbf.is_none_or(|start| start <= now)
}

fn kid_ok(kid: &str) -> bool {
    (1..=64).contains(&kid.len()) && kid.bytes().all(|b| b.is_ascii_graphic())
}

impl Auth {
    pub(crate) fn closed() -> Self {
        Self(Policy::Closed, None)
    }

    pub(crate) fn unsecured() -> Self {
        Self(Policy::Unsecured, None)
    }

    /// Configures the immutable deployment identity for encrypted peer calls.
    #[must_use]
    pub fn with_peer_tls(mut self, tls: crate::PeerTls, transport: crate::PeerTransport) -> Self {
        self.1 = Some((tls, transport));
        self
    }

    pub(crate) fn peer_tls(&self) -> Option<&(crate::PeerTls, crate::PeerTransport)> {
        self.1.as_ref()
    }

    /// Reads explicit authentication configuration without mutating process state.
    ///
    /// # Errors
    /// Missing, conflicting, unknown or invalid settings refuse startup. Errors
    /// never contain configuration values or signing material.
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, AuthError> {
        let config = get("PSTORE_AUTH_CONFIG");
        let private = match get("PSTORE_ALLOW_UNAUTHENTICATED").as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            _ => return Err(AuthError),
        };
        if private {
            return if config.is_none() {
                Ok(Self::unsecured())
            } else {
                Err(AuthError)
            };
        }
        let raw = config.ok_or(AuthError)?;
        if raw.len() > MAX_CONFIG {
            return Err(AuthError);
        }
        let c: Config = serde_json::from_str(&raw).map_err(|_| AuthError)?;
        if !(1..=8).contains(&c.keys.len())
            || c.issuer.is_empty()
            || c.issuer.len() > 1024
            || c.audience.is_empty()
            || c.audience.len() > 1024
            || !kid_ok(&c.active_kid)
        {
            return Err(AuthError);
        }
        let mut keys = BTreeMap::new();
        for key in c.keys {
            if !kid_ok(&key.kid) || key.secret.len() > 2048 {
                return Err(AuthError);
            }
            let secret = base64::engine::general_purpose::STANDARD
                .decode(key.secret)
                .map_err(|_| AuthError)?;
            if !(32..=1024).contains(&secret.len()) {
                return Err(AuthError);
            }
            if keys
                .insert(
                    key.kid,
                    Key {
                        verify: DecodingKey::from_secret(&secret),
                    },
                )
                .is_some()
            {
                return Err(AuthError);
            }
        }
        if !keys.contains_key(&c.active_kid) {
            return Err(AuthError);
        }
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 0;
        validation.validate_nbf = true;
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.set_issuer(&[&c.issuer]);
        validation.set_audience(&[&c.audience]);
        Ok(Self(
            Policy::Bearer(Arc::new(Keyring { keys, validation })),
            None,
        ))
    }

    fn verify(&self, headers: &HeaderMap) -> Result<Principal, ApiError> {
        self.verify_with_time(headers, now)
    }

    fn verify_with_time(
        &self,
        headers: &HeaderMap,
        clock: impl Fn() -> Result<u64, AuthError>,
    ) -> Result<Principal, ApiError> {
        let ring = match &self.0 {
            Policy::Closed => return Err(unauthenticated()),
            Policy::Unsecured => return Ok(Principal::Unsecured),
            Policy::Bearer(ring) => ring,
        };
        let mut credentials = headers.get_all(axum::http::header::AUTHORIZATION).iter();
        let value = credentials.next().ok_or_else(unauthenticated)?;
        if credentials.next().is_some() || value.len() > MAX_TOKEN + 7 {
            return Err(unauthenticated());
        }
        let (scheme, token) = value
            .to_str()
            .ok()
            .and_then(|v| v.split_once(' '))
            .ok_or_else(unauthenticated)?;
        if !scheme.eq_ignore_ascii_case("Bearer") || token.len() > MAX_TOKEN || token.is_empty() {
            return Err(unauthenticated());
        }
        // Check the clock before entering a library that assumes Unix time.
        let _utc = clock().map_err(|_| unauthenticated())?;
        let header = jsonwebtoken::decode_header(token).map_err(|_| unauthenticated())?;
        if header.alg != Algorithm::HS256 {
            return Err(unauthenticated());
        }
        let key = header
            .kid
            .as_ref()
            .and_then(|id| ring.keys.get(id))
            .ok_or_else(unauthenticated)?;
        let claims = jsonwebtoken::decode::<Claims>(token, &key.verify, &ring.validation)
            .map_err(|_| unauthenticated())?
            .claims;
        if !active_at(
            claims.exp,
            claims.nbf,
            clock().map_err(|_| unauthenticated())?,
        ) || claims.sources.len() > 64
            || claims.scopes.len() > 5
        {
            return Err(unauthenticated());
        }
        let tenant = decimal(&claims.tenant).ok_or_else(unauthenticated)?;
        let mut sources = Vec::with_capacity(claims.sources.len());
        for source in claims.sources {
            if source.store.len() > 256 {
                return Err(unauthenticated());
            }
            sources.push((
                decimal(&source.tenant).ok_or_else(unauthenticated)?,
                source.store,
            ));
        }
        Ok(Principal::Verified {
            tenant,
            scopes: claims.scopes,
            sources,
        })
    }
}

fn unauthenticated() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
        "a valid bearer credential is required",
    )
}

fn forbidden() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "forbidden",
        "credential does not authorize this operation",
    )
}

impl Principal {
    fn allows(&self, scope: Scope) -> bool {
        match self {
            Self::Unsecured => true,
            Self::Verified { scopes, .. } => scopes.contains(&scope),
        }
    }

    pub(crate) fn source(&self, tenant: TenantId, store: &str) -> Result<(), ApiError> {
        let allowed = match self {
            Self::Unsecured => true,
            Self::Verified {
                tenant: own,
                scopes,
                sources,
            } => {
                if tenant == *own && store.is_empty() {
                    scopes.contains(&Scope::Read)
                } else {
                    sources.iter().any(|(t, s)| *t == tenant && s == store)
                }
            }
        };
        if allowed { Ok(()) } else { Err(forbidden()) }
    }
}

pub(crate) async fn authenticate(
    State(auth): State<Auth>,
    mut req: Request,
    next: Next,
) -> Response {
    let principal = match auth.verify(req.headers()) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    if let Principal::Verified { tenant, .. } = &principal {
        for value in req.headers().get_all("x-pstore-tenant") {
            if value.to_str().ok().and_then(decimal) != Some(*tenant) {
                return forbidden().into_response();
            }
        }
        let Ok(value) = HeaderValue::from_str(&tenant.0.to_string()) else {
            return forbidden().into_response();
        };
        req.headers_mut().insert("x-pstore-tenant", value);
    }
    req.extensions_mut().insert(principal);
    next.run(req).await
}

async fn require_scope(State(scope): State<Scope>, req: Request, next: Next) -> Response {
    match req.extensions().get::<Principal>() {
        Some(p) if p.allows(scope) => next.run(req).await,
        _ => forbidden().into_response(),
    }
}

/// A route must carry authority before it can enter the serving router.
pub(crate) struct ScopedRoute<S>(MethodRouter<S>);

impl Scope {
    pub(crate) fn on<S: Clone + Send + Sync + 'static>(
        self,
        route: MethodRouter<S>,
    ) -> ScopedRoute<S> {
        ScopedRoute(route.route_layer(axum::middleware::from_fn_with_state(self, require_scope)))
    }
}

pub(crate) struct ScopedRouter<S>(axum::Router<S>);

impl<S: Clone + Send + Sync + 'static> ScopedRouter<S> {
    pub(crate) fn new() -> Self {
        Self(axum::Router::new())
    }

    pub(crate) fn route(self, path: &str, route: ScopedRoute<S>) -> Self {
        Self(self.0.route(path, route.0))
    }

    pub(crate) fn finish(self) -> axum::Router<S> {
        self.0
    }
}

#[cfg(test)]
mod peer_redaction_tests;
#[cfg(test)]
mod tests;
