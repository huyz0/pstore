//! The turbopuffer compatibility facade, part 1 (M268): the vendor's paths, each request
//! translated into **one native call** and its answer translated back.
//!
//! ⚠️ **No blob request and no round trip of its own.** Every handler here rewrites a body,
//! calls the native handler with it, and rewrites what that answered. A facade feature that
//! needed a read the native call does not make belongs in the native API first.
//!
//! The shapes are the pinned SDK's, `turbopuffer` 3.1.0 (`types/namespace_*`). What the
//! facade does not translate yet is refused naming the key, never ignored.

use crate::auth::Principal;
use crate::{Api, ApiError, ListParams};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use pstore_blob::BlobStore;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// The most bytes of a native answer the facade reads back: the native body limit's order.
const MAX_ANSWER: usize = 256 << 20;

/// The document fields of a native write's row; every other key of a vendor row is an
/// attribute (as `upsert_columns` reads a column, M174).
const DOCUMENT_FIELDS: [&str; 4] = ["vector", "vectors", "text", crate::SPARSE_FIELD];

/// The write keys passed to the native write unchanged.
const WRITE_PASSED: [&str; 5] = [
    "upsert_columns",
    "patch_columns",
    "deletes",
    "distance_metric",
    "schema",
];

/// The query keys passed to the native query unchanged.
const QUERY_PASSED: [&str; 6] = [
    "filters",
    "include_attributes",
    "exclude_attributes",
    "aggregate_by",
    "group_by",
    "vector_encoding",
];

fn bad(why: impl Into<String>) -> ApiError {
    ApiError::bad_request(why.into())
}

/// The headers a native handler reads, the tenant set from the facade's own identity rule:
/// a secured server's credential named it already (`auth::authenticate`); an unsecured one's
/// bearer value is the tenant. ⚠️ An unsecured request's own `X-Pstore-Tenant` is refused,
/// never read: on a facade route the tenant is the API key's.
pub(crate) fn identity(
    principal: &Principal,
    mut headers: HeaderMap,
) -> Result<HeaderMap, ApiError> {
    if matches!(principal, Principal::Verified { .. }) {
        return Ok(headers);
    }
    if headers.contains_key("x-pstore-tenant") {
        return Err(bad(
            "X-Pstore-Tenant is not read on a turbopuffer route: the API key names the tenant",
        ));
    }
    let tenant = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|t| t.parse::<u128>().ok())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "an unsecured server's turbopuffer API key is the tenant, a decimal",
            )
        })?;
    let value = HeaderValue::from_str(&tenant.to_string())
        .map_err(|_| bad("the tenant is not a header value"))?;
    headers.insert("x-pstore-tenant", value);
    Ok(headers)
}

/// `body` as a JSON object, or why it is not one.
fn object(body: &[u8]) -> Result<Map<String, Value>, ApiError> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(o)) => Ok(o),
        Ok(_) => Err(bad("the body is a JSON object")),
        Err(e) => Err(bad(format!("malformed body: {e}"))),
    }
}

/// A vendor write as the native write's body (M268, Delta 3).
pub(crate) fn write_body(body: &[u8]) -> Result<Value, ApiError> {
    let mut o = object(body)?;
    let mut out = Map::new();
    out.insert("durability".into(), "durable".into());
    if let Some(rows) = o.remove("upsert_rows") {
        let Value::Array(rows) = rows else {
            return Err(bad("upsert_rows is an array of rows"));
        };
        let documents = rows
            .into_iter()
            .map(|r| match r {
                Value::Object(r) => Ok(document(r)),
                _ => Err(bad("each of upsert_rows is an object")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        out.insert("documents".into(), documents.into());
    }
    if let Some(rows) = o.remove("patch_rows") {
        let Value::Array(rows) = rows else {
            return Err(bad("patch_rows is an array of rows"));
        };
        let patches = rows
            .into_iter()
            .map(|r| match r {
                Value::Object(r) => Ok(patch(r)),
                _ => Err(bad("each of patch_rows is an object")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        out.insert("patch_rows".into(), patches.into());
    }
    // A hint with no pstore meaning: pstore bounds buffered rows, not unfolded backlog.
    o.remove("disable_backpressure");
    for (k, v) in o {
        if !WRITE_PASSED.contains(&k.as_str()) {
            return Err(bad(format!(
                "{k:?} is not translated by this facade yet: the native API has it, at \
                 PUT /v1/indexes/{{index}}/documents"
            )));
        }
        out.insert(k, v);
    }
    Ok(Value::Object(out))
}

/// A vendor row as a native document: `id` and the document fields at the top, every other
/// key an attribute, a `null` one left out (an upsert replaces the row).
fn document(row: Map<String, Value>) -> Value {
    let mut doc = Map::new();
    let mut attrs = Map::new();
    for (k, v) in row {
        if k == "id" || DOCUMENT_FIELDS.contains(&k.as_str()) {
            doc.insert(k, v);
        } else if !v.is_null() {
            attrs.insert(k, v);
        }
    }
    doc.insert("attributes".into(), attrs.into());
    Value::Object(doc)
}

/// A vendor patch row as a native patch: `id` and the document fields at the top (where the
/// native write refuses them, naming why), every other key an attribute, `null` included: a
/// patch's `null` removes the attribute.
fn patch(row: Map<String, Value>) -> Value {
    let mut doc = Map::new();
    let mut attrs = Map::new();
    for (k, v) in row {
        if k == "id" || DOCUMENT_FIELDS.contains(&k.as_str()) {
            doc.insert(k, v);
        } else {
            attrs.insert(k, v);
        }
    }
    doc.insert("attributes".into(), attrs.into());
    Value::Object(doc)
}

/// A vendor query as the native query's body (M268, Delta 4).
pub(crate) fn query_body(body: &[u8]) -> Result<Value, ApiError> {
    let mut o = object(body)?;
    let mut out = Map::new();
    if let Some(rank) = o.remove("rank_by") {
        match rank.as_array().map(Vec::as_slice) {
            Some([field, op, vector]) if matches!(op.as_str(), Some("ANN" | "kNN")) => {
                let field = field
                    .as_str()
                    .ok_or_else(|| bad("an ANN rank_by names its vector field"))?;
                if field != pstore_format::DEFAULT_FIELD {
                    out.insert("field".into(), field.into());
                }
                out.insert("vector".into(), vector.clone());
                if op.as_str() == Some("kNN") {
                    out.insert("exact".into(), true.into());
                }
            }
            _ => {
                out.insert("rank_by".into(), rank);
            }
        }
    }
    match (o.remove("limit"), o.remove("top_k")) {
        (Some(_), Some(_)) => return Err(bad("limit and top_k are the same: name one")),
        (Some(Value::Object(mut l)), None) => {
            let total = l
                .remove("total")
                .ok_or_else(|| bad("limit's object holds total"))?;
            out.insert("top_k".into(), total);
            if let Some(per) = l.remove("per") {
                out.insert("limit_per".into(), per);
            }
            if let Some(k) = l.keys().next() {
                return Err(bad(format!("limit holds total and per, not {k:?}")));
            }
        }
        (Some(n), None) | (None, Some(n)) => {
            out.insert("top_k".into(), n);
        }
        (None, None) => {}
    }
    // The vendor's default: a strong read.
    let level = match o.remove("consistency") {
        None | Some(Value::Null) => json!({"consistency": "strong"}),
        Some(Value::Object(mut c)) => {
            match c.remove("level").as_ref().and_then(Value::as_str) {
                Some("strong") if c.is_empty() => json!({"consistency": "strong"}),
                Some("eventual") => {
                    let secs =
                        match c.remove("max_staleness_seconds") {
                            None => 60,
                            Some(v) => v.as_u64().filter(|s| (1..=3600).contains(s)).ok_or_else(
                                || bad("max_staleness_seconds is a whole number from 1 to 3600"),
                            )?,
                        };
                    if let Some(k) = c.keys().next() {
                        return Err(bad(format!("consistency does not hold {k:?}")));
                    }
                    json!({"consistency": "bounded", "max_staleness_ms": secs * 1000})
                }
                _ => return Err(bad("consistency is {\"level\": \"strong\" | \"eventual\"}")),
            }
        }
        Some(_) => return Err(bad("consistency is an object")),
    };
    if let Value::Object(level) = level {
        out.extend(level);
    }
    for (k, v) in o {
        if !QUERY_PASSED.contains(&k.as_str()) {
            return Err(bad(format!(
                "{k:?} is not translated by this facade yet: the native API has it, at \
                 POST /v1/indexes/{{index}}/query"
            )));
        }
        out.insert(k, v);
    }
    Ok(Value::Object(out))
}

/// A native query's answer as the vendor's (M268, Delta 4).
pub(crate) fn query_answer(native: Value) -> Value {
    let mut out = Map::new();
    let rows: Vec<Value> = native
        .get("results")
        .and_then(Value::as_array)
        .map(|rs| rs.iter().map(row).collect())
        .unwrap_or_default();
    if native.get("results").is_some() {
        out.insert("rows".into(), rows.into());
    }
    for k in ["aggregations", "aggregation_groups"] {
        if let Some(v) = native.get(k).filter(|v| !v.is_null()) {
            out.insert(k.into(), v.clone());
        }
    }
    let p = native.pointer("/meta/performance").unwrap_or(&Value::Null);
    out.insert(
        "performance".into(),
        json!({
            "server_total_ms": p["server_total_ms"].as_f64().map_or(0, |ms| ms.round() as u64),
            "query_execution_ms": p["query_execution_ms"].as_f64().map_or(0, |ms| ms.round() as u64),
            "cache_hit_ratio": p["cache_hit_ratio"].as_f64().unwrap_or(0.0),
        }),
    );
    Value::Object(out)
}

/// A native result row as a vendor row: its attributes flattened beside `id`, and `$dist`
/// the native distance, or the native score where the ranking has none (BM25, an order).
fn row(r: &Value) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), r["id"].clone());
    if let Some(v) = r.get("vector") {
        out.insert("vector".into(), v.clone());
    }
    if let Some(Value::Object(attrs)) = r.get("attributes") {
        for (k, v) in attrs {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some(d) = r.get("$dist").or_else(|| r.get("score")) {
        out.insert("$dist".into(), d.clone());
    }
    Value::Object(out)
}

/// A native write's answer as the vendor's (M268, Delta 3).
pub(crate) fn write_answer(native: &Value) -> Value {
    let n = |k: &str| native[k].as_u64().unwrap_or(0);
    let (up, patched, deleted) = (
        n("documents_written"),
        n("documents_patched"),
        n("documents_deleted"),
    );
    json!({
        "status": "OK",
        "message": format!("{} rows affected", up + patched + deleted),
        "rows_affected": up + patched + deleted,
        "rows_upserted": up,
        "rows_patched": patched,
        "rows_deleted": deleted,
    })
}

/// A native summary as the vendor's namespace metadata (M268, Delta 5).
pub(crate) fn metadata_answer(native: &Value) -> Value {
    let updating = native["unfolded"].as_bool().unwrap_or(false);
    json!({
        "approx_row_count": native["approx_row_count"],
        "approx_logical_bytes": 0,
        "created_at": native["created_at"],
        "updated_at": native["updated_at"],
        "schema": native.pointer("/schema/attributes").cloned().unwrap_or(Value::Null),
        "index": if updating { json!({"status": "updating"}) } else { json!({"status": "up-to-date"}) },
    })
}

/// A native list's answer as the vendor's.
pub(crate) fn list_answer(native: &Value) -> Value {
    let namespaces: Vec<Value> = native["indexes"]
        .as_array()
        .map(|a| a.iter().map(|id| json!({"id": id})).collect())
        .unwrap_or_default();
    json!({"namespaces": namespaces, "next_cursor": native["next_cursor"]})
}

/// A native refusal as the vendor's envelope: `{"status": "error", "error"}`. ⚠️ A 409 that
/// is not `retryable` answers 400: the SDK resends a 409 four times, and pstore's permanent
/// 409s (`index_exists`, `index_read_only`, `index_limit`) would cost five requests each.
pub(crate) fn error_answer(status: StatusCode, native: &Value) -> (StatusCode, Value) {
    let e = &native["error"];
    let retryable = e["retryable"].as_bool().unwrap_or(false);
    let status = if status == StatusCode::CONFLICT && !retryable {
        StatusCode::BAD_REQUEST
    } else {
        status
    };
    let code = e["code"].as_str().unwrap_or("error");
    let message = e["message"].as_str().unwrap_or_default();
    (
        status,
        json!({"status": "error", "error": format!("{code}: {message}")}),
    )
}

/// `answer` translated by `ok` when it succeeded and by [`error_answer`] when it did not,
/// its headers (a `Retry-After`) kept.
async fn translate(
    answer: Result<Response, ApiError>,
    ok: impl FnOnce(Value) -> Value,
) -> Response {
    let res = answer.unwrap_or_else(IntoResponse::into_response);
    let status = res.status();
    let (parts, body) = res.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_ANSWER).await else {
        return (
            status,
            axum::Json(json!({"status": "error", "error": "unreadable answer"})),
        )
            .into_response();
    };
    let native: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let (status, out) = if status.is_success() {
        (status, ok(native))
    } else {
        error_answer(status, &native)
    };
    let mut res = (status, axum::Json(out)).into_response();
    if let Some(after) = parts.headers.get(axum::http::header::RETRY_AFTER) {
        res.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, after.clone());
    }
    res
}

/// `GET /v1/namespaces`.
pub(crate) async fn list<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
    Query(params): Query<ListParams>,
) -> Response {
    let headers = match identity(&principal, headers) {
        Ok(h) => h,
        Err(e) => return translate(Err(e), |v| v).await,
    };
    let answer = crate::list_indexes(State(api), headers, Query(params))
        .await
        .map(IntoResponse::into_response);
    translate(answer, |v| list_answer(&v)).await
}

/// `POST /v2/namespaces/{ns}`: a write.
pub(crate) async fn write<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
    body: Bytes,
) -> Response {
    let prepared = identity(&principal, headers).and_then(|h| write_body(&body).map(|b| (h, b)));
    let answer = match prepared {
        Ok((headers, native)) => {
            crate::write_documents(
                State(api),
                Path(ns),
                headers,
                axum::Extension(principal),
                Bytes::from(native.to_string()),
            )
            .await
        }
        Err(e) => Err(e),
    };
    translate(answer, |v| write_answer(&v)).await
}

/// `DELETE /v2/namespaces/{ns}`.
pub(crate) async fn delete_all<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
) -> Response {
    let answer = match identity(&principal, headers) {
        Ok(headers) => crate::delete_index(State(api), Path(ns), headers)
            .await
            .map(IntoResponse::into_response),
        Err(e) => Err(e),
    };
    translate(answer, |_| json!({"status": "OK"})).await
}

/// `POST /v2/namespaces/{ns}/query`: one query.
pub(crate) async fn query<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
    body: Bytes,
) -> Response {
    let prepared = identity(&principal, headers).and_then(|h| query_body(&body).map(|b| (h, b)));
    let answer = match prepared {
        Ok((headers, native)) => {
            crate::query_index(
                State(api),
                Path(ns),
                headers,
                Bytes::from(native.to_string()),
            )
            .await
        }
        Err(e) => Err(e),
    };
    translate(answer, query_answer).await
}

/// `GET /v1/namespaces/{ns}/schema`: the attributes' declarations, as metadata's `schema`.
/// The SDK's `exists()` reads it.
pub(crate) async fn schema<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
) -> Response {
    let answer = match identity(&principal, headers) {
        Ok(headers) => crate::index_summary(State(api), Path(ns), Query(HashMap::new()), headers)
            .await
            .map(IntoResponse::into_response),
        Err(e) => Err(e),
    };
    translate(answer, |v| {
        metadata_answer(&v)
            .get("schema")
            .cloned()
            .unwrap_or(Value::Null)
    })
    .await
}

/// `GET /v2/namespaces/{ns}/metadata`.
pub(crate) async fn metadata<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
) -> Response {
    let answer = match identity(&principal, headers) {
        Ok(headers) => crate::index_summary(State(api), Path(ns), Query(HashMap::new()), headers)
            .await
            .map(IntoResponse::into_response),
        Err(e) => Err(e),
    };
    translate(answer, |v| metadata_answer(&v)).await
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn query(body: &Value) -> Value {
        query_body(body.to_string().as_bytes()).unwrap()
    }

    /// Delta 4: `kNN` is the native exhaustive search, and `ANN` is not.
    #[test]
    fn knn_is_exact_and_ann_is_not() {
        let knn = query(&json!({"rank_by": ["vector", "kNN", [1.0, 0.0]], "top_k": 2}));
        assert_eq!(knn["exact"], true, "{knn}");
        assert_eq!(knn["vector"], json!([1.0, 0.0]), "{knn}");
        let ann = query(&json!({"rank_by": ["vector", "ANN", [1.0, 0.0]], "top_k": 2}));
        assert!(ann.get("exact").is_none(), "{ann}");
        // The default field is the native default, unnamed; another is named.
        assert!(ann.get("field").is_none(), "{ann}");
        let named = query(&json!({"rank_by": ["title_v", "ANN", [1.0, 0.0]], "top_k": 2}));
        assert_eq!(named["field"], "title_v", "{named}");
    }

    /// Delta 6: only a permanent 409 becomes 400; a retryable 409 and a 503 keep their
    /// status, so the SDK resends what pstore says to resend.
    #[test]
    fn only_a_permanent_conflict_is_answered_400() {
        let native = |code: &str, retryable: bool| json!({"error": {"code": code, "message": "m", "retryable": retryable}});
        for (status, code, retryable, want) in [
            (
                StatusCode::CONFLICT,
                "index_exists",
                false,
                StatusCode::BAD_REQUEST,
            ),
            (
                StatusCode::CONFLICT,
                "version_conflict",
                true,
                StatusCode::CONFLICT,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                true,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            let (got, body) = error_answer(status, &native(code, retryable));
            assert_eq!(got, want, "{code}");
            assert_eq!(
                body,
                json!({"status": "error", "error": format!("{code}: m")}),
                "{code}"
            );
        }
    }

    /// Delta 4: `eventual` is `bounded` by the staleness given, in milliseconds, 60 s by
    /// default; no consistency is `strong`.
    #[test]
    fn eventual_is_bounded_in_milliseconds() {
        let at = |c: Value| query(&json!({"rank_by": ["id", "asc"], "top_k": 1, "consistency": c}));
        let five = at(json!({"level": "eventual", "max_staleness_seconds": 5}));
        assert_eq!(
            (&five["consistency"], &five["max_staleness_ms"]),
            (&json!("bounded"), &json!(5000)),
            "{five}"
        );
        let default = at(json!({"level": "eventual"}));
        assert_eq!(default["max_staleness_ms"], 60_000, "{default}");
        let none = query(&json!({"rank_by": ["id", "asc"], "top_k": 1}));
        assert_eq!(none["consistency"], "strong", "{none}");
        assert_eq!(at(json!({"level": "strong"}))["consistency"], "strong");
        // A strong level holds nothing else.
        let refused = query_body(
            json!({"top_k": 1, "consistency": {"level": "strong", "max_staleness_seconds": 5}})
                .to_string()
                .as_bytes(),
        );
        assert!(refused.is_err(), "{refused:?}");
    }
}
