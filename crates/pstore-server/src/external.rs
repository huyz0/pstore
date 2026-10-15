//! External indexes (M86): the HTTP surface. The engine registers, queries and drops
//! ([`pstore_engine::Engine::register_external`]); this parses the wire and maps each refusal.
//!
//! ⚠️ **Off unless the deployment says so** (`PSTORE_EXTERNAL_SOURCES=allow`, with the prefixes
//! of the buckets a tenant may point at in `PSTORE_EXTERNAL_PREFIXES`). OQ-169 answered that
//! reading a customer's object is feasible; it did not grant permission to.

use crate::types::{Cost, WarmMeta};
use crate::{Api, ApiError, tenant_of};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use pstore_blob::BlobStore;
use pstore_engine::ExternalKind;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The most rows one query may ask for.
const MAX_K: usize = 1000;
/// The most rows one query may have read from the customer's object: each is a ranged GET, and
/// the window is what the tenant's query costs the customer's bucket.
const MAX_WINDOW: usize = 10_000;
/// The window when the request names none: four rows read for each returned.
const WINDOW_PER_K: usize = 4;

/// `PUT /v1/indexes/{index}/external`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterBody {
    kind: Kind,
    source: String,
    column: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Parquet,
    Lance,
}

/// `POST /v1/indexes/{index}/external/query`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryBody {
    vector: Vec<f32>,
    k: Option<usize>,
    window: Option<usize>,
}

/// A registration's or a drop's answer: the epoch that committed, and what it cost.
#[derive(Debug, Serialize)]
pub(crate) struct Committed {
    epoch: u64,
    cost: Cost,
}

#[derive(Debug, Serialize)]
pub(crate) struct Hit {
    row: usize,
    score: f32,
}

#[derive(Debug, Serialize)]
pub(crate) struct Answer {
    hits: Vec<Hit>,
    meta: WarmMeta,
}

/// The rows read from the customer's object when a request names no window.
fn default_window(k: usize) -> usize {
    k * WINDOW_PER_K
}

fn body<T: for<'de> Deserialize<'de>>(raw: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(raw)
        .map_err(|e| ApiError::bad_request(format!("not an external index request: {e}")))
}

pub(crate) async fn register<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<axum::Json<Committed>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let req: RegisterBody = body(&raw)?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    let kind = match req.kind {
        Kind::Parquet => ExternalKind::Parquet,
        Kind::Lance => ExternalKind::Lance,
    };
    let epoch = engine
        .register_external(&index, kind, &req.source, &req.column)
        .await?;
    Ok(axum::Json(Committed {
        epoch: epoch.0,
        cost: api.spend(tenant).since(before),
    }))
}

pub(crate) async fn query<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<axum::Json<Answer>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let req: QueryBody = body(&raw)?;
    let k = req.k.unwrap_or(10);
    if !(1..=MAX_K).contains(&k) {
        return Err(ApiError::bad_request(format!(
            "k must be between 1 and {MAX_K}"
        )));
    }
    let window = req.window.unwrap_or_else(|| default_window(k));
    if !(k..=MAX_WINDOW).contains(&window) {
        return Err(ApiError::bad_request(format!(
            "window must be between k and {MAX_WINDOW}"
        )));
    }
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    let hits = engine
        .query_external(&index, &req.vector, k, window)
        .await?;
    Ok(axum::Json(Answer {
        hits: hits
            .into_iter()
            .map(|(row, score)| Hit { row, score })
            .collect(),
        meta: WarmMeta {
            cost: api.spend(tenant).since(before),
        },
    }))
}

pub(crate) async fn drop_index<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(index): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<Committed>, ApiError> {
    let tenant = tenant_of(&headers)?;
    let engine = api.engine(tenant).await;
    let before = api.spend(tenant);
    let Some(epoch) = engine.drop_external(&index).await? else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "external_index_not_found",
            format!("this tenant has no external index {index}"),
        ));
    };
    Ok(axum::Json(Committed {
        epoch: epoch.0,
        cost: api.spend(tenant).since(before),
    }))
}

#[cfg(test)]
mod tests {
    use super::{MAX_K, MAX_WINDOW, WINDOW_PER_K, default_window};

    #[test]
    fn an_unnamed_window_is_four_rows_a_hit_and_always_within_the_bound() {
        assert_eq!(WINDOW_PER_K, 4);
        assert_eq!(default_window(1), 4);
        assert_eq!(default_window(3), 12);
        assert_eq!(default_window(10), 40);
        // Even the largest k's default is a window the request would have been allowed to name.
        assert!(default_window(MAX_K) <= MAX_WINDOW);
    }
}
