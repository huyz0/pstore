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
const WRITE_PASSED: [&str; 11] = [
    "upsert_columns",
    "patch_columns",
    "deletes",
    "distance_metric",
    "schema",
    // M269: by filter and by condition.
    "delete_by_filter",
    "upsert_condition",
    "patch_condition",
    "delete_condition",
    "delete_by_filter_allow_partial",
    "patch_by_filter_allow_partial",
];

/// The write keys whose answer is the rows the write changed (M269, Delta 3): a write
/// carrying one is `applied`, since only an applied write knows them.
const WRITE_APPLIED: [&str; 7] = [
    "delete_by_filter",
    "patch_by_filter",
    "upsert_condition",
    "patch_condition",
    "delete_condition",
    "delete_by_filter_allow_partial",
    "patch_by_filter_allow_partial",
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
    out.insert("durability".into(), durability(&o).into());
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
    // M269: the vendor's `patch` is the native `attributes`.
    if let Some(by) = o.remove("patch_by_filter") {
        out.insert("patch_by_filter".into(), patch_by_filter(by)?);
    }
    match o.remove("return_affected_ids") {
        None | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => {
            out.insert("affected_ids".into(), true.into());
        }
        Some(_) => return Err(bad("return_affected_ids is a boolean")),
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

/// The durability a vendor write needs (M269, Delta 3): `applied` when its answer is the rows
/// it changed -- a condition, a by-filter operation, or its affected ids -- else `durable`.
fn durability(o: &Map<String, Value>) -> &'static str {
    let ids = o.get("return_affected_ids") == Some(&Value::Bool(true));
    if ids
        || WRITE_APPLIED
            .iter()
            .any(|k| o.get(*k).is_some_and(|v| !v.is_null()))
    {
        "applied"
    } else {
        "durable"
    }
}

/// The vendor's `patch_by_filter: {"filters", "patch"}` as the native `{"filters",
/// "attributes"}`.
fn patch_by_filter(by: Value) -> Result<Value, ApiError> {
    let Value::Object(mut by) = by else {
        return Err(bad("patch_by_filter is {\"filters\", \"patch\"}"));
    };
    let filters = by
        .remove("filters")
        .ok_or_else(|| bad("patch_by_filter holds filters"))?;
    let patch = by
        .remove("patch")
        .ok_or_else(|| bad("patch_by_filter holds patch"))?;
    if let Some(k) = by.keys().next() {
        return Err(bad(format!(
            "patch_by_filter holds filters and patch, not {k:?}"
        )));
    }
    Ok(json!({"filters": filters, "attributes": patch}))
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
///
/// A body holding `queries` is a multi-query (M269, Delta 5), as the SDK's `multi_query`
/// sends it to the same path.
pub(crate) fn query_body(body: &[u8]) -> Result<Value, ApiError> {
    let o = object(body)?;
    if o.contains_key("queries") {
        multi_body(o)
    } else {
        query_of(o)
    }
}

/// A vendor multi-query as the native `{"queries"}` (M269, Delta 5): each sub-query
/// translated as one query is, under the root's `consistency` and `vector_encoding`.
fn multi_body(mut o: Map<String, Value>) -> Result<Value, ApiError> {
    let Some(Value::Array(subs)) = o.remove("queries") else {
        return Err(bad("queries is an array of queries"));
    };
    let shared: Vec<(String, Value)> = ["consistency", "vector_encoding"]
        .into_iter()
        .filter_map(|k| o.remove(k).map(|v| (k.to_owned(), v)))
        .collect();
    if let Some(k) = o.keys().next() {
        return Err(bad(format!(
            "{k:?} is not translated by this facade yet: a multi-query holds queries, \
             consistency and vector_encoding"
        )));
    }
    let queries = subs
        .into_iter()
        .map(|q| {
            let Value::Object(mut q) = q else {
                return Err(bad("each of queries is an object"));
            };
            if q.contains_key("consistency") {
                return Err(bad(
                    "a multi-query's consistency is its root's: a query inside queries \
                     does not carry one",
                ));
            }
            for (k, v) in &shared {
                q.entry(k.clone()).or_insert_with(|| v.clone());
            }
            query_of(q)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"queries": queries}))
}

/// One vendor query, parsed, as the native query's body (M268, Delta 4).
fn query_of(mut o: Map<String, Value>) -> Result<Value, ApiError> {
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
    out.insert("performance".into(), performance(&native));
    Value::Object(out)
}

/// A native multi-query's answer as the vendor's (M269, Delta 5): each query's rows and
/// aggregations, in order.
pub(crate) fn multi_answer(native: &Value) -> Value {
    let results: Vec<Value> = native["results"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .enumerate()
                .map(|(i, ranked)| {
                    let mut r = Map::new();
                    let rows: Vec<Value> = ranked
                        .as_array()
                        .map(|rows| rows.iter().map(row).collect())
                        .unwrap_or_default();
                    r.insert("rows".into(), rows.into());
                    for k in ["aggregations", "aggregation_groups"] {
                        if let Some(v) = native
                            .get(k)
                            .and_then(|a| a.get(i))
                            .filter(|v| !v.is_null())
                        {
                            r.insert(k.into(), v.clone());
                        }
                    }
                    Value::Object(r)
                })
                .collect()
        })
        .unwrap_or_default();
    json!({"results": results, "performance": performance(native)})
}

/// The native M264 performance block as the vendor's.
fn performance(native: &Value) -> Value {
    let p = native.pointer("/meta/performance").unwrap_or(&Value::Null);
    json!({
            "server_total_ms": p["server_total_ms"].as_f64().map_or(0, |ms| ms.round() as u64),
            "query_execution_ms": p["query_execution_ms"].as_f64().map_or(0, |ms| ms.round() as u64),
            "cache_hit_ratio": p["cache_hit_ratio"].as_f64().unwrap_or(0.0),
    })
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
///
/// An applied write (M269, Delta 4) answers its outcome: the rows changed, `rows_remaining`,
/// and each non-empty list of affected ids.
pub(crate) fn write_answer(native: &Value) -> Result<Value, (StatusCode, Value)> {
    let applied = native.get("applied");
    if applied.is_some_and(Value::is_null) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"status": "error", "error": "outcome_unavailable: the write applied, and \
                what it changed can no longer be read"}),
        ));
    }
    let n = |k: &str| native[k].as_u64().unwrap_or(0);
    let (up, patched, deleted) = match applied {
        Some(a) => {
            let c = |k: &str| a[k].as_u64().unwrap_or(0);
            (c("upserted"), c("patched"), c("deleted"))
        }
        None => (
            n("documents_written"),
            n("documents_patched"),
            n("documents_deleted"),
        ),
    };
    let mut out = Map::new();
    for (k, v) in [
        ("status", json!("OK")),
        (
            "message",
            json!(format!("{} rows affected", up + patched + deleted)),
        ),
        ("rows_affected", json!(up + patched + deleted)),
        ("rows_upserted", json!(up)),
        ("rows_patched", json!(patched)),
        ("rows_deleted", json!(deleted)),
    ] {
        out.insert(k.to_owned(), v);
    }
    if let Some(a) = applied {
        if let Some(r) = a.get("rows_remaining") {
            out.insert("rows_remaining".to_owned(), r.clone());
        }
        for (native_key, vendor_key) in [
            ("upserted", "upserted_ids"),
            ("patched", "patched_ids"),
            ("deleted", "deleted_ids"),
        ] {
            if let Some(ids) = a
                .pointer(&format!("/ids/{native_key}"))
                .filter(|v| v.as_array().is_some_and(|l| !l.is_empty()))
            {
                out.insert(vendor_key.to_owned(), ids.clone());
            }
        }
    }
    Ok(Value::Object(out))
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
    translate_or(answer, |v| Ok(ok(v))).await
}

/// [`translate`], with a success the facade may still refuse (M269: an outcome that can no
/// longer be read).
async fn translate_or(
    answer: Result<Response, ApiError>,
    ok: impl FnOnce(Value) -> Result<Value, (StatusCode, Value)>,
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
        ok(native).map_or_else(|refused| refused, |out| (status, out))
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
    translate_or(answer, |v| write_answer(&v)).await
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

/// `POST /v2/namespaces/{ns}/query`: one query, or a multi-query (M269).
pub(crate) async fn query<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
    body: Bytes,
) -> Response {
    let prepared = identity(&principal, headers).and_then(|h| query_body(&body).map(|b| (h, b)));
    let multi = prepared
        .as_ref()
        .is_ok_and(|(_, native)| native.get("queries").is_some());
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
    if multi {
        translate(answer, |v| multi_answer(&v)).await
    } else {
        translate(answer, query_answer).await
    }
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

    /// M269, Delta 3: a write is applied exactly when its answer is the rows it changed.
    #[test]
    fn a_write_is_applied_only_when_its_answer_needs_it() {
        let durability =
            |w: Value| write_body(w.to_string().as_bytes()).unwrap()["durability"].clone();
        let rows = json!([{"id": 1, "vector": [1.0, 0.0]}]);
        for plain in [
            json!({"upsert_rows": rows}),
            json!({"deletes": [1], "patch_rows": [{"id": 1, "n": 2}]}),
            json!({"upsert_rows": rows, "return_affected_ids": false}),
            json!({"upsert_rows": rows, "upsert_condition": null}),
        ] {
            assert_eq!(durability(plain.clone()), "durable", "{plain}");
        }
        for applied in [
            json!({"upsert_rows": rows, "return_affected_ids": true}),
            json!({"upsert_rows": rows, "upsert_condition": ["n", "Eq", 1]}),
            json!({"patch_rows": [{"id": 1, "n": 2}], "patch_condition": ["n", "Eq", 1]}),
            json!({"deletes": [1], "delete_condition": ["n", "Eq", 1]}),
            json!({"delete_by_filter": ["n", "Eq", 1]}),
            json!({"patch_by_filter": {"filters": ["n", "Eq", 1], "patch": {"n": 2}}}),
            json!({"delete_by_filter": ["n", "Eq", 1], "delete_by_filter_allow_partial": true}),
            json!({"patch_by_filter": {"filters": ["n", "Eq", 1], "patch": {"n": 2}},
                "patch_by_filter_allow_partial": true}),
        ] {
            assert_eq!(durability(applied.clone()), "applied", "{applied}");
        }
    }

    #[test]
    fn each_sub_query_takes_the_roots_consistency() {
        let multi = query_body(
            json!({"queries": [{"rank_by": ["id", "asc"], "top_k": 1},
                               {"rank_by": ["n", "desc"], "top_k": 2}],
                   "consistency": {"level": "eventual"}})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        let subs = multi["queries"].as_array().unwrap();
        assert_eq!(subs.len(), 2, "{multi}");
        for q in subs {
            assert_eq!(q["consistency"], "bounded", "{multi}");
        }
    }

    #[test]
    fn an_applied_write_answers_what_changed_not_what_was_sent() {
        let native = json!({"documents_written": 2, "documents_patched": 0,
            "documents_deleted": 2,
            "applied": {"upserted": 1, "patched": 0, "deleted": 1, "skipped": 2, "rejected": 0}});
        let a = write_answer(&native).unwrap();
        assert_eq!(
            (&a["rows_upserted"], &a["rows_deleted"], &a["rows_affected"]),
            (&json!(1), &json!(1), &json!(2)),
            "{a}"
        );
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
