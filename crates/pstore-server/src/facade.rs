//! The turbopuffer compatibility facade, part 1 (M268): the vendor's paths, each request
//! translated into **one native call** and its answer translated back.
//!
//! ⚠️ **No blob request and no round trip of its own.** Every handler here rewrites a body,
//! calls the native handler with it, and rewrites what that answered. A facade feature that
//! needed a read the native call does not make belongs in the native API first.
//!
//! The shapes are the pinned SDK's, `turbopuffer` 3.1.0 (`types/namespace_*`). What the
//! facade does not translate yet is refused naming the key, never ignored.
//!
//! M274: a conditional or by-filter write is two native calls, as a native client keying it
//! makes them: the idempotency key's issue (M132), then the keyed write.

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

/// The keys that name another organization, region or key for a copy's source (M271): pstore's
/// identity reaches none of them.
const REMOTE_SOURCE: [&str; 3] = ["source_api_key", "source_region", "dest_encryption"];

fn refuse_remote(o: &Map<String, Value>) -> Result<(), ApiError> {
    match REMOTE_SOURCE.iter().find(|k| o.contains_key(**k)) {
        Some(k) => Err(bad(format!(
            "{k:?} is not served: a copy reads this tenant's own namespace, in this store"
        ))),
        None => Ok(()),
    }
}

fn source_name(v: Value) -> Result<String, ApiError> {
    match v {
        Value::String(s) => Ok(s),
        _ => Err(bad("source_namespace is a namespace's name")),
    }
}

/// The namespace a vendor write branches or copies from (M271, Delta 1), taken out of `o`:
/// `branch_from_namespace` or `copy_from_namespace`, a name or `{"source_namespace"}`. The
/// SDK's `branch_from`, `copy_from` and `start_copy_from` all send one of these.
fn copy_source(o: &mut Map<String, Value>) -> Result<Option<String>, ApiError> {
    let from = match (
        o.remove("branch_from_namespace"),
        o.remove("copy_from_namespace"),
    ) {
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => {
            return Err(bad(
                "branch_from_namespace and copy_from_namespace are one operation: name one",
            ));
        }
        (Some(v), None) | (None, Some(v)) => v,
    };
    match from {
        Value::String(s) => Ok(Some(s)),
        Value::Object(mut c) => {
            refuse_remote(&c)?;
            let src = c
                .remove("source_namespace")
                .ok_or_else(|| bad("a copy's source holds source_namespace"))?;
            if let Some(k) = c.keys().next() {
                return Err(bad(format!(
                    "a copy's source holds source_namespace, not {k:?}"
                )));
            }
            source_name(src).map(Some)
        }
        _ => Err(bad(
            "a copy's source is a namespace's name or {\"source_namespace\"}",
        )),
    }
}

/// A vendor write as the native write's body (M268, Delta 3). A branch or copy (M271) is the
/// native branch; anything beside it is translated and sent too, for the native write to
/// refuse, since a branch comes alone (M122).
pub(crate) fn write_body(body: &[u8]) -> Result<Value, ApiError> {
    let mut o = object(body)?;
    let source = copy_source(&mut o)?;
    let mut out = translated_write(o)?;
    if let Some(src) = source
        && let Value::Object(m) = &mut out
    {
        m.remove("durability");
        m.insert("branch_from_namespace".into(), src.into());
    }
    Ok(out)
}

/// The vendor write's keys as the native write's.
fn translated_write(mut o: Map<String, Value>) -> Result<Value, ApiError> {
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
    // M275: the vendor's type names as the native ones.
    if let Some(schema) = o.remove("schema") {
        out.insert("schema".into(), schema_declarations(schema));
    }
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
        "approx_logical_bytes": native
            .get("approx_logical_bytes")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| json!(0)),
        "created_at": native["created_at"],
        "updated_at": native["updated_at"],
        "schema": schema_answer(native),
        "index": if updating { json!({"status": "updating"}) } else { json!({"status": "up-to-date"}) },
    })
}

/// A recorded attribute type's vendor name (M275): `uint64` is the vendor's `uint`; the rest
/// share a name. A recorded type holds scalars and arrays of them alike, so it is answered as
/// the scalar's name.
fn vendor_type(native: &str) -> &str {
    match native {
        "uint64" => "uint",
        other => other,
    }
}

/// A vendor attribute type as the native declaration (M275): `uint` is `uint64`, and an array
/// of a recordable kind is that kind, which holds arrays of it (M175). Any other name is
/// passed as written, for the native write to accept or refuse.
fn native_type(vendor: &str) -> &str {
    match vendor.strip_prefix("[]").unwrap_or(vendor) {
        "uint" => "uint64",
        k @ ("uuid" | "datetime") => k,
        _ => vendor,
    }
}

/// The vendor's write `schema` with its type names as the native ones (M275): a name's value,
/// or an object's `type`.
fn schema_declarations(schema: Value) -> Value {
    let Value::Object(m) = schema else {
        return schema;
    };
    let named = |v: Value| match v {
        Value::String(t) => Value::String(native_type(&t).to_owned()),
        // `ann: true` is what pstore does by rule for every dense field (M198), so it asks for
        // nothing; any other `ann` is passed, for the native write to refuse.
        Value::Object(mut o) if o.get("ann") == Some(&Value::Bool(true)) => {
            o.remove("ann");
            named_object(o)
        }
        Value::Object(o) => named_object(o),
        v => v,
    };
    Value::Object(m.into_iter().map(|(k, v)| (k, named(v))).collect())
}

/// [`schema_declarations`]' object form: `{"type": t}` alone is the native `t`, which declares
/// a type only by its name; any other object keeps its keys, its `type` renamed.
fn named_object(mut o: Map<String, Value>) -> Value {
    match o.get("type") {
        Some(Value::String(t)) if o.len() == 1 => Value::String(native_type(t).to_owned()),
        Some(Value::String(t)) => {
            let t = native_type(t).to_owned();
            o.insert("type".into(), t.into());
            Value::Object(o)
        }
        _ => Value::Object(o),
    }
}

/// The native summary's `schema` as the vendor's (M275): every declaration pstore records,
/// with no request of its own. Each dense field is typed `[N]f32`, `[N]f16` or `[N]i8` and
/// ANN-indexed by rule (M198) under the index's metric; each text field carries its analyzer,
/// and no `type`, since pstore does not record whether it holds strings or string arrays; a
/// regex attribute `regex: true`; a recorded attribute its type; and `id` its kind. `null`
/// before the first fold records a schema.
pub(crate) fn schema_answer(native: &Value) -> Value {
    let Some(sc) = native.get("schema").filter(|s| s.is_object()) else {
        return Value::Null;
    };
    let mut out = Map::new();
    let entry = |out: &mut Map<String, Value>, name: &str, k: &str, v: Value| {
        if let Value::Object(e) = out
            .entry(name.to_owned())
            .or_insert_with(|| Value::Object(Map::new()))
        {
            e.insert(k.to_owned(), v);
        }
    };
    let metric = sc["distance_metric"].clone();
    let vector_type = |name: &str, width: &Value| {
        sc.pointer(&format!("/vector_types/{name}"))
            .cloned()
            .unwrap_or_else(|| format!("[{width}]f32").into())
    };
    let dims = &sc["dims"];
    if dims.as_u64().is_some_and(|d| d > 0) {
        entry(&mut out, "vector", "type", vector_type("vector", dims));
        entry(
            &mut out,
            "vector",
            "ann",
            json!({"distance_metric": metric}),
        );
    }
    for (name, width) in sc["vectors"].as_object().into_iter().flatten() {
        entry(&mut out, name, "type", vector_type(name, width));
        entry(&mut out, name, "ann", json!({"distance_metric": metric}));
    }
    if let Some(text) = sc["text_field"].as_str().filter(|t| !t.is_empty()) {
        entry(
            &mut out,
            text,
            "full_text_search",
            sc["full_text_search"].clone(),
        );
    }
    for (name, fts) in sc["text_fields"].as_object().into_iter().flatten() {
        entry(&mut out, name, "full_text_search", fts.clone());
    }
    for name in sc["regex"].as_array().into_iter().flatten() {
        if let Some(name) = name.as_str() {
            entry(&mut out, name, "regex", true.into());
        }
    }
    for (name, ty) in sc["attributes"].as_object().into_iter().flatten() {
        if let Some(ty) = ty.as_str() {
            entry(&mut out, name, "type", vendor_type(ty).into());
        }
    }
    if let Some(id) = sc["id"].as_str() {
        entry(&mut out, "id", "type", vendor_type(id).into());
    }
    Value::Object(out)
}

/// A native list's answer as the vendor's.
pub(crate) fn list_answer(native: &Value) -> Value {
    let namespaces: Vec<Value> = native["indexes"]
        .as_array()
        .map(|a| a.iter().map(|id| json!({"id": id})).collect())
        .unwrap_or_default();
    json!({"namespaces": namespaces, "next_cursor": native["next_cursor"]})
}

/// The vendor's `explain_query` body as the native query asking for its plan (M270, Delta 1).
pub(crate) fn explain_body(body: &[u8]) -> Result<Value, ApiError> {
    let o = object(body)?;
    if o.contains_key("queries") {
        return Err(bad(
            "explain_query explains one query: queries belongs to multi_query",
        ));
    }
    let mut q = query_of(o)?;
    if let Value::Object(m) = &mut q {
        m.insert("explain".into(), true.into());
    }
    Ok(q)
}

/// The native query's plan as the vendor's `plan_text` (M270, Delta 1): `meta.explain` as
/// indented JSON, or `null` when the query has no plan.
pub(crate) fn explain_answer(native: &Value) -> Value {
    let plan = native
        .pointer("/meta/explain")
        .filter(|p| !p.is_null())
        .and_then(|p| serde_json::to_string_pretty(p).ok());
    json!({ "plan_text": plan })
}

/// The native warm's answer as the vendor's hint answer (M270, Delta 2).
pub(crate) fn warm_answer(native: &Value) -> Value {
    json!({"status": "ACCEPTED", "message": format!("warmed {} segments", native["segments"])})
}

/// The keys the native recall takes from the vendor's body unchanged (M270, Delta 3).
const RECALL_PASSED: [&str; 4] = ["num", "top_k", "filters", "include_ground_truth"];

/// The vendor's recall body as the native one (M270, Delta 3).
pub(crate) fn recall_body(body: &[u8]) -> Result<Value, ApiError> {
    let o = object(body)?;
    if let Some(k) = o.keys().find(|k| !RECALL_PASSED.contains(&k.as_str())) {
        return Err(bad(format!(
            "{k:?} is not translated by this facade: recall takes num, top_k, filters and \
             include_ground_truth, and samples its query vectors from the index"
        )));
    }
    Ok(Value::Object(o))
}

/// The native recall's answer as the vendor's (M270, Delta 3): each neighbor's `dist` as
/// `$dist`, and no recall invented where the native one is `null`.
pub(crate) fn recall_answer(native: &Value) -> Value {
    let mut out = Map::new();
    for k in ["avg_recall", "avg_ann_count", "avg_exhaustive_count"] {
        out.insert(k.into(), native.get(k).cloned().unwrap_or(Value::Null));
    }
    if let Some(Value::Array(truth)) = native.get("ground_truth") {
        let truth = truth
            .iter()
            .map(|g| {
                let neighbors: Vec<Value> = g
                    .get("nearest_neighbors")
                    .and_then(Value::as_array)
                    .map(|ns| {
                        ns.iter()
                            .map(|n| json!({"id": n.get("id"), "$dist": n.get("dist")}))
                            .collect()
                    })
                    .unwrap_or_default();
                json!({"query_vector": g.get("query_vector"), "nearest_neighbors": neighbors})
            })
            .collect();
        out.insert("ground_truth".into(), Value::Array(truth));
    }
    Value::Object(out)
}

/// The vendor's `update_metadata` body as the native index patch, or `None` when it asks
/// for nothing (M270, Delta 5).
pub(crate) fn metadata_patch(body: &[u8]) -> Result<Option<Value>, ApiError> {
    let mut o = object(body)?;
    match o.remove("pinning") {
        None | Some(Value::Null | Value::Bool(false)) => {}
        Some(_) => {
            return Err(bad(
                "pinning is not served: pstore pins no namespace, so pinning takes only null \
                 or false",
            ));
        }
    }
    let read_only = match o.remove("read_only") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(b),
        Some(_) => return Err(bad("read_only is true or false")),
    };
    if let Some(k) = o.keys().next() {
        return Err(bad(format!(
            "{k:?} is not translated by this facade: update_metadata takes read_only and pinning"
        )));
    }
    Ok(read_only.map(|r| json!({ "read_only": r })))
}

/// The vendor's `start_copy_from` body as the native copy's (M271, Delta 2).
pub(crate) fn copy_body(body: &[u8]) -> Result<Value, ApiError> {
    let mut o = object(body)?;
    let src = copy_source(&mut o)?
        .ok_or_else(|| bad("start_copy_from names its source in copy_from_namespace"))?;
    if let Some(k) = o.keys().next() {
        return Err(bad(format!(
            "{k:?} is not translated by this facade: start_copy_from takes copy_from_namespace"
        )));
    }
    Ok(json!({"source": {"index": src}}))
}

/// The native copy's run, as the vendor's operation token (M271, Delta 2).
pub(crate) fn copy_token(native: &Value) -> Value {
    json!({"token": native["run"].to_string()})
}

/// The native copy's status as the vendor's operation (M271, Delta 3), or 404 when `token`
/// is not its run.
pub(crate) fn operation_answer(native: &Value, token: &str) -> Result<Value, (StatusCode, Value)> {
    if native["run"].as_u64().is_none() || native["run"].as_u64() != token.parse().ok() {
        return Err((
            StatusCode::NOT_FOUND,
            json!({"status": "error", "error": format!(
                "operation_not_found: {token:?} is not this namespace's copy"
            )}),
        ));
    }
    let start = native["created_at"].clone();
    let state = native["state"].as_str().unwrap_or_default();
    if matches!(state, "running" | "paused") {
        let progress = native
            .get("progress")
            .filter(|p| !p.is_null())
            .map(|p| format!("{} of {} segments", p["segments_done"], p["segments_total"]));
        return Ok(json!({"status": "running", "start_time": start, "progress": progress}));
    }
    let result = if state == "succeeded" {
        json!({"success": {"status": "OK", "message": "copied", "rows_affected": 0}})
    } else {
        let code = native["code"].as_str().unwrap_or(state);
        let status_code = if code == "source_not_found" { 404 } else { 409 };
        let error = match native["last_error"].as_str() {
            Some(last) => format!("{code}: {last}"),
            None => code.to_owned(),
        };
        json!({"error": {"status_code": status_code,
            "detail": {"status": "error", "error": error}}})
    };
    Ok(json!({"status": "finished", "start_time": start,
        "finish_time": native["ended_at"], "result": result}))
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

/// The SDK's attempt number (M274): `x-stainless-retry-count`, `0` on a first attempt. A
/// request without it, or with one that is not a number, is a first attempt.
fn attempt_of(headers: &HeaderMap) -> u64 {
    headers
        .get("x-stainless-retry-count")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// The most first attempts a server remembers (M274).
pub(crate) const ATTEMPTS_MAX: usize = 4096;

/// The idempotency keys this server gave first attempts of conditional and by-filter writes
/// (M274), by two keyed 64-bit hashes of tenant, namespace and body; oldest evicted first.
pub(crate) struct Attempts {
    hashers: [std::hash::RandomState; 2],
    inner: std::sync::Mutex<AttemptsInner>,
}

/// Two independently keyed 64-bit hashes: 128 bits, kept apart so no arithmetic packs them.
type Digest = [u64; 2];

#[derive(Default)]
struct AttemptsInner {
    keys: HashMap<Digest, String>,
    order: std::collections::VecDeque<Digest>,
}

impl Default for Attempts {
    fn default() -> Self {
        Self {
            hashers: [std::hash::RandomState::new(), std::hash::RandomState::new()],
            inner: std::sync::Mutex::default(),
        }
    }
}

impl std::fmt::Debug for Attempts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attempts")
            .field("remembered", &self.lock().order.len())
            .finish_non_exhaustive()
    }
}

impl Attempts {
    /// The body's hash: two independently keyed 64-bit hashes of tenant, namespace and bytes.
    fn digest(&self, tenant: u128, ns: &str, body: &[u8]) -> Digest {
        use std::hash::BuildHasher;
        self.hashers
            .each_ref()
            .map(|s| s.hash_one((tenant, ns, body)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AttemptsInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Remembers `key` for `digest`, evicting the oldest past [`ATTEMPTS_MAX`].
    fn remember(&self, digest: Digest, key: String) {
        let mut m = self.lock();
        if m.keys.insert(digest, key).is_none() {
            m.order.push_back(digest);
        }
        // One in at most, so one out at most: no loop a mutant can make endless.
        if m.order.len() > ATTEMPTS_MAX
            && let Some(old) = m.order.pop_front()
        {
            m.keys.remove(&old);
        }
    }

    fn key(&self, digest: Digest) -> Option<String> {
        self.lock().keys.get(&digest).cloned()
    }
}

/// Whether a native write is evaluated against the rows at its fold (M274): a condition or a
/// by-filter operation. A resend of any other writes the same rows.
fn evaluated(native: &Value) -> bool {
    WRITE_APPLIED
        .iter()
        .any(|k| native.get(*k).is_some_and(|v| !v.is_null()))
}

/// `native` with the idempotency key its attempt needs (M274): a first attempt's fresh key,
/// remembered; a resend's remembered one; or a refusal when a resend cannot be matched.
async fn keyed<S: BlobStore + 'static>(
    api: &Api<S>,
    ns: &str,
    headers: &HeaderMap,
    attempt: u64,
    body: &[u8],
    mut native: Value,
) -> Result<Value, ApiError> {
    if !evaluated(&native) {
        return Ok(native);
    }
    let tenant = crate::tenant_of(headers)?;
    let keys = api.registry_mut().idempotency;
    let digest = api.attempts.digest(tenant.0, ns, body);
    let key = if attempt == 0 {
        if !keys {
            return Ok(native);
        }
        let random = crate::operation_id()?.0;
        let key = api.engine(tenant).await.idempotency_key(random).await?;
        api.attempts.remember(digest, key.to_string());
        key.to_string()
    } else {
        match api.attempts.key(digest).filter(|_| keys) {
            Some(key) => key,
            None => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "retry_outcome_unknown",
                    format!(
                        "this is resend {attempt} of a conditional or by-filter write whose \
                         first attempt this server cannot match{}; it may have applied, so \
                         read the rows, then call again",
                        if keys {
                            ""
                        } else {
                            " (idempotency keys are off: PSTORE_APPLIED_WRITES=3)"
                        }
                    ),
                ));
            }
        }
    };
    if let Value::Object(m) = &mut native {
        m.insert("idempotency_key".into(), key.into());
    }
    Ok(native)
}

/// `POST /v2/namespaces/{ns}`: a write.
pub(crate) async fn write<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
    body: Bytes,
) -> Response {
    let attempt = attempt_of(&headers);
    let prepared = identity(&principal, headers).and_then(|h| write_body(&body).map(|b| (h, b)));
    let prepared = match prepared {
        Ok((h, native)) => keyed(&api, &ns, &h, attempt, &body, native)
            .await
            .map(|n| (h, n)),
        Err(e) => Err(e),
    };
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

/// `POST /v2/namespaces/{ns}/explain_query` (M270, Delta 1).
pub(crate) async fn explain<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
    body: Bytes,
) -> Response {
    let prepared = identity(&principal, headers).and_then(|h| explain_body(&body).map(|b| (h, b)));
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
    translate(answer, |v| explain_answer(&v)).await
}

/// `GET /v1/namespaces/{ns}/hint_cache_warm` (M270, Delta 2). With no read cache there is
/// nothing to warm, and the hint is still accepted.
pub(crate) async fn warm<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
) -> Response {
    let answer = match identity(&principal, headers) {
        Ok(headers) => crate::warm_index(State(api), Path(ns), headers)
            .await
            .map(IntoResponse::into_response),
        Err(e) => Err(e),
    };
    if let Err(e) = &answer
        && e.code == "no_read_cache"
    {
        return axum::Json(json!({"status": "ACCEPTED",
            "message": "this server has no read cache: nothing was warmed"}))
        .into_response();
    }
    translate(answer, |v| warm_answer(&v)).await
}

/// `POST /v1/namespaces/{ns}/_debug/recall` (M270, Delta 3).
pub(crate) async fn recall<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
    body: Bytes,
) -> Response {
    let prepared = identity(&principal, headers).and_then(|h| recall_body(&body).map(|b| (h, b)));
    let answer = match prepared {
        Ok((headers, native)) => crate::recall_index(
            State(api),
            Path(ns),
            headers,
            Bytes::from(native.to_string()),
        )
        .await
        .map(IntoResponse::into_response),
        Err(e) => Err(e),
    };
    translate(answer, |v| recall_answer(&v)).await
}

/// `POST /v1/namespaces/{ns}/schema` (M270, Delta 4): the native schema patch, which refuses.
pub(crate) async fn update_schema(
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
) -> Response {
    let refused = match identity(&principal, headers) {
        Ok(headers) => crate::patch_schema(Path(ns), headers).await,
        Err(e) => e,
    };
    translate(Err(refused), |v| v).await
}

/// `PATCH /v1/namespaces/{ns}/metadata` (M270, Delta 5): the native patch when it asks for
/// one, then the native summary, which is what the vendor answers.
pub(crate) async fn update_metadata<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
    body: Bytes,
) -> Response {
    let (headers, patch) =
        match identity(&principal, headers).and_then(|h| metadata_patch(&body).map(|p| (h, p))) {
            Ok(prepared) => prepared,
            Err(e) => return translate(Err(e), |v| v).await,
        };
    if let Some(patch) = patch {
        let patched = crate::patch_index(
            State(Arc::clone(&api)),
            Path(ns.clone()),
            headers.clone(),
            Bytes::from(patch.to_string()),
        )
        .await;
        if let Err(e) = patched {
            return translate(Err(e), |v| v).await;
        }
    }
    let answer = crate::index_summary(State(api), Path(ns), Query(HashMap::new()), headers)
        .await
        .map(IntoResponse::into_response);
    translate(answer, |v| metadata_answer(&v)).await
}

/// `POST /v2/namespaces/{ns}/async`: `start_copy_from` (M271, Delta 2).
pub(crate) async fn start_copy<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
    body: Bytes,
) -> Response {
    let prepared = identity(&principal, headers).and_then(|h| copy_body(&body).map(|b| (h, b)));
    let answer = match prepared {
        Ok((headers, native)) => {
            crate::replication::copy(
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
    let mut res = translate(answer, |v| copy_token(&v)).await;
    // The vendor answers a started copy 200; the native copy answers 202.
    if res.status() == StatusCode::ACCEPTED {
        *res.status_mut() = StatusCode::OK;
    }
    res
}

/// `GET /v1/namespaces/{ns}/operations/{token}`: `poll_copy_from` (M271, Delta 3).
pub(crate) async fn poll_copy<S: BlobStore + 'static>(
    State(api): State<Arc<Api<S>>>,
    Path((ns, token)): Path<(String, String)>,
    headers: HeaderMap,
    axum::Extension(principal): axum::Extension<Principal>,
) -> Response {
    let answer = match identity(&principal, headers) {
        Ok(headers) => crate::replication::copy_status(State(api), Path(ns), headers)
            .await
            .map(IntoResponse::into_response),
        Err(e) => Err(e),
    };
    translate_or(answer, |v| operation_answer(&v, &token)).await
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    /// M274: at most [`ATTEMPTS_MAX`] remembered, the oldest evicted first; a body's digest is
    /// its tenant's and namespace's too.
    #[test]
    fn the_attempt_memory_is_bounded() {
        let a = Attempts::default();
        let d = |i: usize| a.digest(7, "docs", i.to_string().as_bytes());
        for i in 0..ATTEMPTS_MAX {
            a.remember(d(i), format!("k{i}"));
        }
        assert_eq!(a.key(d(0)).as_deref(), Some("k0"));
        // Remembering a body again replaces its key without taking a second place.
        a.remember(d(1), "k1'".to_owned());
        a.remember(d(ATTEMPTS_MAX), "new".to_owned());
        assert_eq!(a.key(d(0)), None, "the oldest evicted");
        assert_eq!(a.key(d(1)).as_deref(), Some("k1'"));
        assert_eq!(
            a.key(d(ATTEMPTS_MAX)).as_deref(),
            Some("new"),
            "the newest kept"
        );
        assert_eq!(a.lock().keys.len(), ATTEMPTS_MAX);
        assert_eq!(a.lock().order.len(), ATTEMPTS_MAX);
        assert_ne!(d(1), a.digest(8, "docs", b"1"), "another tenant");
        assert_ne!(d(1), a.digest(7, "other", b"1"), "another namespace");
        assert_eq!(attempt_of(&HeaderMap::new()), 0);
        let mut h = HeaderMap::new();
        h.insert("x-stainless-retry-count", HeaderValue::from_static("3"));
        assert_eq!(attempt_of(&h), 3);
        h.insert("x-stainless-retry-count", HeaderValue::from_static("x"));
        assert_eq!(attempt_of(&h), 0);
    }

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
    fn an_operation_is_the_native_copys_state() {
        let status = |state: &str, code: Value, last: Value| {
            json!({"run": 3, "state": state, "code": code, "last_error": last,
                "created_at": "2026-11-16T00:00:00.000Z", "ended_at": "2026-11-16T00:00:09.000Z",
                "progress": {"segments_done": 1, "segments_total": 4}})
        };
        let running = operation_answer(&status("running", Value::Null, Value::Null), "3").unwrap();
        assert_eq!(
            running,
            json!({"status": "running", "start_time": "2026-11-16T00:00:00.000Z",
                "progress": "1 of 4 segments"})
        );
        let paused = operation_answer(&status("paused", Value::Null, Value::Null), "3").unwrap();
        assert_eq!(paused["status"], "running", "{paused}");
        let done = operation_answer(&status("succeeded", Value::Null, Value::Null), "3").unwrap();
        assert_eq!(done["status"], "finished", "{done}");
        assert_eq!(done["finish_time"], "2026-11-16T00:00:09.000Z", "{done}");
        assert_eq!(done["result"]["success"]["status"], "OK", "{done}");
        for (state, code, last, status_code, error) in [
            (
                "failed",
                json!("source_not_found"),
                json!("gone"),
                404,
                "source_not_found: gone",
            ),
            (
                "failed",
                json!("rebuild_refused"),
                Value::Null,
                409,
                "rebuild_refused",
            ),
            ("cancelled", Value::Null, Value::Null, 409, "cancelled"),
        ] {
            let a = operation_answer(&status(state, code, last), "3").unwrap();
            assert_eq!(a["status"], "finished", "{a}");
            assert_eq!(
                a["result"]["error"],
                json!({"status_code": status_code, "detail": {"status": "error", "error": error}}),
                "{a}"
            );
        }
        let (code, other) =
            operation_answer(&status("running", Value::Null, Value::Null), "4").unwrap_err();
        assert_eq!(code, StatusCode::NOT_FOUND, "{other}");
    }

    /// M272: each manifest key's status is what the translation does with it.
    #[test]
    fn the_manifest_keys_are_the_translation() {
        let m: Value =
            serde_json::from_str(include_str!("../../../api/compat/turbopuffer.json")).unwrap();
        let untranslated = |r: Result<Value, ApiError>, k: &str| match r {
            Err(e) => e.message.contains(&format!("{k:?} is not translated")),
            Ok(_) => false,
        };
        for (keys, translate) in [
            (
                "write_keys",
                write_body as fn(&[u8]) -> Result<Value, ApiError>,
            ),
            ("query_keys", query_body),
        ] {
            let keys = m[keys].as_object().unwrap();
            assert!(keys.len() >= 14, "{keys:?}");
            for (k, entry) in keys {
                let refused = untranslated(translate(json!({ k: 1 }).to_string().as_bytes()), k);
                let status = entry["status"].as_str().unwrap();
                assert!(
                    matches!(status, "refused" | "served" | "hint"),
                    "{k}: {status}"
                );
                if status == "refused" {
                    assert!(refused, "{k} is marked refused and is translated");
                } else {
                    assert!(!refused, "{k} is marked {status} and is refused");
                }
            }
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

    /// M275: only `ann: true` is dropped, as asking for nothing; any other `ann` reaches the
    /// native write, which refuses it.
    #[test]
    fn only_a_true_ann_is_dropped() {
        let out = schema_declarations(json!({
            "v": {"type": "[3]f32", "ann": true},
            "w": {"type": "[3]f32", "ann": false},
            "n": {"type": "uint", "ann": {"distance_metric": "cosine_distance"}},
        }));
        assert_eq!(
            out,
            json!({
                "v": "[3]f32",
                "w": {"type": "[3]f32", "ann": false},
                "n": {"type": "uint64", "ann": {"distance_metric": "cosine_distance"}},
            })
        );
    }
}
