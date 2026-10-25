//! Tenant identity, endpoint scopes and source authority through the HTTP boundary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use base64::Engine as _;
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_server::{Api, Auth};
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

pub(crate) const KEY: [u8; 64] = [42; 64];
pub(crate) const OTHER_KEY: [u8; 64] = [43; 64];

pub(crate) fn config(keys: &[(&str, &[u8])], active: &str) -> Value {
    json!({
        "issuer": "pstore-tests", "audience": "pstore-test-api", "active_kid": active,
        "keys": keys.iter().map(|(kid, secret)| json!({
            "kid": kid, "secret": base64::engine::general_purpose::STANDARD.encode(secret)
        })).collect::<Vec<_>>()
    })
}

pub(crate) fn auth(config: &Value) -> Auth {
    Auth::from_vars(|name| (name == "PSTORE_AUTH_CONFIG").then(|| config.to_string())).unwrap()
}

pub(crate) fn api(config: &Value) -> (Arc<Api<MemoryStore>>, Accounted<MemoryStore>) {
    let store = Accounted::new(MemoryStore::new());
    let api = Api::with_auth(store.clone(), LaneId(1), auth(config)).unwrap();
    (api, store)
}

pub(crate) fn claims(tenant: &str, scopes: &[&str]) -> Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    json!({"iss":"pstore-tests", "aud":"pstore-test-api", "exp":now + 3600,
        "tenant":tenant, "scopes":scopes, "sources":[]})
}

pub(crate) fn sign(claims: &Value, kid: Option<&str>, algorithm: Algorithm, key: &[u8]) -> String {
    let mut header = Header::new(algorithm);
    header.kid = kid.map(str::to_owned);
    encode(&header, claims, &EncodingKey::from_secret(key)).unwrap()
}

pub(crate) fn token(tenant: &str, scopes: &[&str]) -> String {
    sign(
        &claims(tenant, scopes),
        Some("current"),
        Algorithm::HS256,
        &KEY,
    )
}

pub(crate) struct Answer {
    pub(crate) status: StatusCode,
    pub(crate) body: Value,
    pub(crate) text: String,
    pub(crate) headers: HeaderMap,
}

pub(crate) async fn send(
    api: &Arc<Api<MemoryStore>>,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    tenant: Option<&str>,
    body: &Value,
) -> Answer {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(bearer) = bearer {
        req = req.header("authorization", format!("Bearer {bearer}"));
    }
    if let Some(tenant) = tenant {
        req = req.header("x-pstore-tenant", tenant);
    }
    let res = api
        .clone()
        .router()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text: String::from_utf8(bytes.to_vec()).unwrap(),
        headers,
    }
}

pub(crate) fn requests(store: &Accounted<MemoryStore>) -> [u64; 4] {
    [
        OpClass::Read,
        OpClass::Write,
        OpClass::List,
        OpClass::Delete,
    ]
    .map(|c| store.total(c))
}
