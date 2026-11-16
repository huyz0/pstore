//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The turbopuffer facade, part 1 (M268): each vendor request one native call, its answer
//! the native answer's translation.

use crate::support::auth;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_server::Api;
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

async fn call<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    method: &str,
    uri: &str,
    headers: &[(&str, String)],
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let mut r = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        r = r.header(*k, v);
    }
    let r = r
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(r).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn key(tenant: u128) -> Vec<(&'static str, String)> {
    vec![("authorization", format!("Bearer {tenant}"))]
}

fn native(tenant: u128) -> Vec<(&'static str, String)> {
    vec![("x-pstore-tenant", tenant.to_string())]
}

fn rows() -> Value {
    json!([
        {"id": 1, "vector": [1.0, 0.0], "title": "red fox jumps", "n": 3, "kind": "a"},
        {"id": 2, "vector": [0.8, 0.6], "title": "lazy dog sleeps", "n": 1, "kind": "a"},
        {"id": 3, "vector": [0.0, 1.0], "title": "red red dog", "n": 2, "kind": "b"},
        {"id": 4, "vector": [0.6, 0.8], "title": "quick fox", "n": 5, "kind": "b", "gone": null}
    ])
}

async fn written<S: BlobStore + 'static>(api: &Arc<Api<S>>, tenant: u128) {
    let (s, b) = call(
        api,
        "POST",
        "/v2/namespaces/docs",
        &key(tenant),
        Some(
            &json!({"upsert_rows": rows(), "distance_metric": "euclidean_squared",
            "disable_backpressure": true,
            "schema": {"title": {"type": "string", "full_text_search": true}}}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "OK", "{b}");
    assert_eq!(b["rows_affected"], 4, "{b}");
    assert_eq!(b["rows_upserted"], 4, "{b}");
    let (s, b) = call(
        api,
        "POST",
        "/v1/admin/fold",
        &native(tenant),
        Some(&json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// The facade's rows, compared with the native rows they translate.
fn same_rows(facade: &Value, native: &Value, dist: &str) {
    let f = facade["rows"].as_array().unwrap();
    let n = native["results"].as_array().unwrap();
    assert_eq!(f.len(), n.len(), "{facade} vs {native}");
    assert!(!f.is_empty(), "{facade}");
    for (f, n) in f.iter().zip(n) {
        assert_eq!(f["id"], n["id"], "{f} vs {n}");
        assert_eq!(f["$dist"], n[dist], "{f} vs {n}");
        for (k, v) in n["attributes"].as_object().into_iter().flatten() {
            assert_eq!(&f[k], v, "{k}: {f} vs {n}");
        }
        assert!(f.get("attributes").is_none(), "flattened: {f}");
    }
}

#[tokio::test]
async fn facade_write_and_query_are_native_calls() {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    written(&api, 7).await;
    let pairs = [
        (
            json!({"rank_by": ["vector", "ANN", [1.0, 0.1]], "limit": 3,
                "include_attributes": true}),
            json!({"vector": [1.0, 0.1], "top_k": 3, "include_attributes": true,
                "consistency": "strong"}),
            "$dist",
        ),
        (
            json!({"rank_by": ["vector", "kNN", [0.1, 1.0]], "top_k": 2,
                "include_attributes": ["n"]}),
            json!({"vector": [0.1, 1.0], "top_k": 2, "include_attributes": ["n"],
                "exact": true, "consistency": "strong"}),
            "$dist",
        ),
        (
            json!({"rank_by": ["title", "BM25", "red fox"], "limit": 10,
                "include_attributes": ["title"]}),
            json!({"rank_by": ["title", "BM25", "red fox"], "top_k": 10,
                "include_attributes": ["title"], "consistency": "strong"}),
            "score",
        ),
        (
            json!({"rank_by": ["n", "desc"], "limit": 10, "include_attributes": ["n"],
                "filters": ["kind", "Eq", "b"]}),
            json!({"rank_by": ["n", "desc"], "top_k": 10, "include_attributes": ["n"],
                "filters": ["kind", "Eq", "b"], "consistency": "strong"}),
            "score",
        ),
        (
            json!({"rank_by": ["vector", "ANN", [1.0, 0.0]],
                "limit": {"total": 4, "per": {"attributes": ["kind"], "limit": 1}},
                "include_attributes": ["kind"]}),
            json!({"vector": [1.0, 0.0], "top_k": 4,
                "limit_per": {"attributes": ["kind"], "limit": 1},
                "include_attributes": ["kind"], "consistency": "strong"}),
            "$dist",
        ),
        (
            json!({"rank_by": ["vector", "ANN", [1.0, 0.0]], "limit": 2,
                "consistency": {"level": "eventual", "max_staleness_seconds": 5}}),
            json!({"vector": [1.0, 0.0], "top_k": 2, "consistency": "bounded",
                "max_staleness_ms": 5000}),
            "$dist",
        ),
    ];
    for (f, n, dist) in pairs {
        let (s, fb) = call(&api, "POST", "/v2/namespaces/docs/query", &key(7), Some(&f)).await;
        assert_eq!(s, StatusCode::OK, "{f} -> {fb}");
        let (s, nb) = call(&api, "POST", "/v1/indexes/docs/query", &native(7), Some(&n)).await;
        assert_eq!(s, StatusCode::OK, "{n} -> {nb}");
        same_rows(&fb, &nb, dist);
        let p = &fb["performance"];
        assert!(
            p["server_total_ms"].is_u64() && p["query_execution_ms"].is_u64(),
            "{fb}"
        );
        assert!(
            fb.get("billing").is_none(),
            "billing is never manufactured: {fb}"
        );
    }
    // An aggregation passes through.
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/docs/query",
        &key(7),
        Some(&json!({"aggregate_by": {"c": ["Count"]}, "group_by": ["kind"]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["aggregation_groups"].as_array().unwrap().len(), 2, "{b}");
    // A null attribute is one the row does not hold.
    let (_, b) = call(
        &api,
        "POST",
        "/v1/indexes/docs/query",
        &native(7),
        Some(
            &json!({"rank_by": ["id", "asc"], "top_k": 10, "include_attributes": true,
            "filters": ["id", "Eq", 4], "consistency": "strong"}),
        ),
    )
    .await;
    assert!(b["results"][0]["attributes"].get("gone").is_none(), "{b}");
    // Patch and delete, then what the native query sees.
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/docs",
        &key(7),
        Some(&json!({"patch_rows": [{"id": 1, "n": 9}], "deletes": [2]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (&b["rows_patched"], &b["rows_deleted"], &b["rows_affected"]),
        (&json!(1), &json!(1), &json!(2)),
        "{b}"
    );
    let (_, b) = call(&api, "POST", "/v1/admin/fold", &native(7), Some(&json!({}))).await;
    assert!(b.is_object());
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/docs/query",
        &key(7),
        Some(&json!({"rank_by": ["id", "asc"], "limit": 10, "include_attributes": ["n"]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let ids: Vec<&Value> = b["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["id"])
        .collect();
    assert_eq!(ids, [&json!(1), &json!(3), &json!(4)], "{b}");
    assert_eq!(b["rows"][0]["n"], 9, "{b}");
    // Listing, over pages, and metadata.
    call(
        &api,
        "POST",
        "/v2/namespaces/more",
        &key(7),
        Some(&json!({"upsert_rows": [{"id": 1, "vector": [1.0, 0.0]}],
            "distance_metric": "cosine_distance"})),
    )
    .await;
    let (s, b) = call(&api, "GET", "/v1/namespaces?page_size=1", &key(7), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["namespaces"], json!([{"id": "docs"}]), "{b}");
    let cursor = b["next_cursor"].as_str().unwrap().to_owned();
    let (_, b) = call(
        &api,
        "GET",
        &format!("/v1/namespaces?page_size=1&cursor={cursor}"),
        &key(7),
        None,
    )
    .await;
    assert_eq!(b["namespaces"], json!([{"id": "more"}]), "{b}");
    let (s, b) = call(&api, "GET", "/v2/namespaces/docs/metadata", &key(7), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["approx_row_count"], 3, "{b}");
    assert!(
        b["created_at"].is_string() && b["schema"].is_object(),
        "{b}"
    );
    assert_eq!(b["index"]["status"], "up-to-date", "{b}");
    // Deleted, then gone.
    let (s, b) = call(&api, "DELETE", "/v2/namespaces/docs", &key(7), None).await;
    assert_eq!((s, &b), (StatusCode::OK, &json!({"status": "OK"})), "{b}");
    let (s, _) = call(&api, "GET", "/v2/namespaces/docs/metadata", &key(7), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn facade_errors_are_the_vendors() {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    // One index a tenant, set before its engine is built: `made` is that one.
    api.limit_indexes(1);
    let (s, b) = call(
        &api,
        "GET",
        "/v2/namespaces/nothing/metadata",
        &key(7),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["status"], "error", "{b}");
    assert!(
        b["error"]
            .as_str()
            .unwrap()
            .starts_with("index_not_found: "),
        "{b}"
    );
    // A permanent 409 answers 400, so the SDK does not resend it.
    let (s, b) = call(
        &api,
        "PUT",
        "/v1/indexes/made",
        &native(7),
        Some(&json!({"dims": 2, "distance_metric": "cosine_distance"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = call(
        &api,
        "PUT",
        "/v1/indexes/made",
        &native(7),
        Some(&json!({"dims": 2, "distance_metric": "cosine_distance"})),
    )
    .await;
    assert_eq!(
        (s, &b["error"]["code"], &b["error"]["retryable"]),
        (StatusCode::CONFLICT, &json!("index_exists"), &json!(false)),
        "{b}"
    );
    // The facade's own route to one: a write past the tenant's index limit.
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/another",
        &key(7),
        Some(&json!({"upsert_rows": [{"id": 1, "vector": [1.0, 0.0]}],
            "distance_metric": "cosine_distance"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["status"], "error", "{b}");
    assert!(
        b["error"].as_str().unwrap().starts_with("index_limit: "),
        "{b}"
    );
    // Keys a later part translates are refused, named.
    for (path, body, says) in [
        (
            "/v2/namespaces/made",
            json!({"copy_from_namespace": "other"}),
            "\"copy_from_namespace\"",
        ),
        (
            "/v2/namespaces/made",
            json!({"upsert_rows": [], "encryption": {}}),
            "\"encryption\"",
        ),
        (
            "/v2/namespaces/made/query",
            json!({"rank_by": ["id", "asc"], "rerank_by": ["RRF"]}),
            "\"rerank_by\"",
        ),
        (
            "/v2/namespaces/made/query",
            json!({"limit": 1, "top_k": 1}),
            "name one",
        ),
        (
            "/v2/namespaces/made/query",
            json!({"consistency": {"level": "eventual",
            "max_staleness_seconds": 0}}),
            "max_staleness_seconds",
        ),
        ("/v2/namespaces/made", json!([1]), "a JSON object"),
    ] {
        let (s, b) = call(&api, "POST", path, &key(7), Some(&body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
        assert!(b["error"].as_str().unwrap().contains(says), "{body} -> {b}");
    }
}

#[tokio::test]
async fn facade_identity_never_reads_the_tenant_header() {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    written(&api, 7).await;
    // The API key is the tenant: tenant 8 has no `docs`.
    let (s, _) = call(&api, "GET", "/v2/namespaces/docs/metadata", &key(8), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&api, "GET", "/v2/namespaces/docs/metadata", &key(7), None).await;
    assert_eq!(s, StatusCode::OK);
    // A tenant header is refused, never read; no key, or one not a tenant, is unauthenticated.
    let mut both = key(8);
    both.extend(native(7));
    let (s, b) = call(&api, "GET", "/v2/namespaces/docs/metadata", &both, None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["error"].as_str().unwrap().contains("X-Pstore-Tenant"),
        "{b}"
    );
    let (s, _) = call(
        &api,
        "GET",
        "/v2/namespaces/docs/metadata",
        &native(7),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&api, "GET", "/v2/namespaces/docs/metadata", &[], None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // Only plain decimal digits name a tenant: a sign, which `u128`'s parser would take, or
    // nothing at all, is not one.
    for k in ["Bearer seven", "Bearer +7", "Bearer "] {
        let (s, _) = call(
            &api,
            "GET",
            "/v2/namespaces/docs/metadata",
            &[("authorization", k.to_owned())],
            None,
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{k}");
    }
    // Secured: the credential's tenant, under the native scopes.
    let config = auth::config(&[("current", &auth::KEY)], "current");
    let (secured, _) = auth::api(&config);
    let bearer = |tenant: &str, scopes: &[&str]| {
        vec![(
            "authorization",
            format!("Bearer {}", auth::token(tenant, scopes)),
        )]
    };
    let (s, b) = call(
        &secured,
        "POST",
        "/v2/namespaces/s",
        &bearer("7", &["write"]),
        Some(&json!({"upsert_rows": [{"id": 1, "vector": [1.0, 0.0]}]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, _) = call(
        &secured,
        "GET",
        "/v2/namespaces/s/metadata",
        &bearer("8", &["read"]),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(
        &secured,
        "GET",
        "/v2/namespaces/s/metadata",
        &bearer("7", &["read"]),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        &secured,
        "GET",
        "/v2/namespaces/s/metadata",
        &bearer("7", &["write"]),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let mut other = bearer("7", &["read"]);
    other.extend(native(8));
    let (s, _) = call(&secured, "GET", "/v2/namespaces/s/metadata", &other, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn facade_adds_no_request() {
    // Each call measured in a world of its own, built the same way, so neither side sees
    // what the other wrote: requests grow with an index's history.
    let cost_of = async |m: &str, p: &str, h: &[(&str, String)], b: &Value| {
        let depth = DepthCounting::new(MemoryStore::new());
        let store = Accounted::new(depth.clone());
        let api = applied_api(store.clone());
        let spent =
            || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(7), c));
        written(&api, 7).await;
        let before = spent();
        depth.reset();
        let (s, body) = call(&api, m, p, h, Some(b)).await;
        assert_eq!(s, StatusCode::OK, "{p}: {body}");
        let after = spent();
        (
            before
                .iter()
                .zip(after)
                .map(|(b, a)| a - b)
                .collect::<Vec<_>>(),
            depth.depth(),
        )
    };
    let mut pairs = Vec::new();
    for (path, f, n) in [
        (
            "/query",
            json!({"rank_by": ["vector", "ANN", [1.0, 0.1]], "limit": 3}),
            json!({"vector": [1.0, 0.1], "top_k": 3, "consistency": "strong"}),
        ),
        (
            "",
            json!({"upsert_rows": [{"id": 9, "vector": [1.0, 0.0], "n": 1}],
                "distance_metric": "euclidean_squared"}),
            json!({"durability": "durable", "distance_metric": "euclidean_squared",
                "documents": [{"id": 9, "vector": [1.0, 0.0], "attributes": {"n": 1}}]}),
        ),
        // M269: a conditional write, applied, and a two-query multi-query.
        (
            "",
            json!({"patch_rows": [{"id": 1, "n": 4}], "patch_condition": ["n", "Lt", 9],
                "return_affected_ids": true}),
            json!({"durability": "applied", "affected_ids": true,
                "patch_rows": [{"id": 1, "attributes": {"n": 4}}],
                "patch_condition": ["n", "Lt", 9]}),
        ),
        (
            "/query",
            json!({"queries": [{"rank_by": ["vector", "ANN", [1.0, 0.1]], "limit": 3},
                {"rank_by": ["n", "asc"], "limit": 2}]}),
            json!({"queries": [{"vector": [1.0, 0.1], "top_k": 3, "consistency": "strong"},
                {"rank_by": ["n", "asc"], "top_k": 2, "consistency": "strong"}]}),
        ),
    ] {
        let (method, native_path) = if path.is_empty() {
            ("PUT", "/v1/indexes/docs/documents".to_owned())
        } else {
            ("POST", format!("/v1/indexes/docs{path}"))
        };
        let cost = [
            cost_of("POST", &format!("/v2/namespaces/docs{path}"), &key(7), &f).await,
            cost_of(method, &native_path, &native(7), &n).await,
        ];
        pairs.push(cost);
    }
    for cost in pairs {
        assert_eq!(cost[0], cost[1], "facade then native: {cost:?}");
    }
}

// M269: writes by filter and condition, affected ids, multi-query.

/// An unsecured server serving every applied-write level, as `scripts/facade.sh` runs one.
fn applied_api<S: BlobStore + 'static>(store: Accounted<S>) -> Arc<Api<S>> {
    let api = Api::unsecured(store, LaneId(1)).unwrap();
    api.set_applied_writes(true);
    api.set_affected_ids(true);
    api.set_idempotency(true);
    api.set_partial_filters(true);
    api.set_filter_ids(true);
    api
}

/// `index` holding [`rows`], written natively and folded.
async fn seeded<S: BlobStore + 'static>(api: &Arc<Api<S>>, index: &str) {
    let documents: Vec<Value> = rows()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let mut attrs = r.as_object().unwrap().clone();
            let id = attrs.remove("id").unwrap();
            let vector = attrs.remove("vector").unwrap();
            json!({"id": id, "vector": vector, "attributes": attrs})
        })
        .collect();
    let (s, b) = call(
        api,
        "PUT",
        &format!("/v1/indexes/{index}/documents"),
        &native(7),
        Some(
            &json!({"durability": "durable", "distance_metric": "euclidean_squared",
            "documents": documents}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = call(api, "POST", "/v1/admin/fold", &native(7), Some(&json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Each row's `n`, by id, as a native strong order reads them.
async fn ns<S: BlobStore + 'static>(api: &Arc<Api<S>>, index: &str) -> Vec<(Value, Value)> {
    let (s, b) = call(
        api,
        "POST",
        &format!("/v1/indexes/{index}/query"),
        &native(7),
        Some(
            &json!({"rank_by": ["id", "asc"], "top_k": 100, "include_attributes": ["n"],
            "consistency": "strong"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].clone(), r["attributes"]["n"].clone()))
        .collect()
}

#[tokio::test]
async fn facade_filters_and_conditions_are_native_calls() {
    let api = applied_api(Accounted::new(MemoryStore::new()));
    // Each case: the vendor's write, the native write it means, and the vendor answer's
    // counts (upserted, patched, deleted) and ids.
    let cases = [
        (
            json!({"delete_by_filter": ["kind", "Eq", "a"]}),
            json!({"durability": "applied", "delete_by_filter": ["kind", "Eq", "a"]}),
            (0, 0, 2),
            json!({}),
        ),
        (
            json!({"patch_by_filter": {"filters": ["kind", "Eq", "b"], "patch": {"n": 50}},
                "return_affected_ids": true}),
            json!({"durability": "applied", "affected_ids": true,
                "patch_by_filter": {"filters": ["kind", "Eq", "b"], "attributes": {"n": 50}}}),
            (0, 2, 0),
            json!({"patched_ids": [3, 4]}),
        ),
        (
            json!({"upsert_rows": [{"id": 1, "vector": [1.0, 0.0], "n": 30},
                    {"id": 2, "vector": [1.0, 0.0], "n": 30}],
                "upsert_condition": ["n", "Gte", 3], "return_affected_ids": true,
                "distance_metric": "euclidean_squared"}),
            json!({"durability": "applied", "affected_ids": true,
                "documents": [{"id": 1, "vector": [1.0, 0.0], "attributes": {"n": 30}},
                    {"id": 2, "vector": [1.0, 0.0], "attributes": {"n": 30}}],
                "upsert_condition": ["n", "Gte", 3], "distance_metric": "euclidean_squared"}),
            (1, 0, 0),
            json!({"upserted_ids": [1]}),
        ),
        (
            json!({"patch_rows": [{"id": 1, "n": 40}, {"id": 2, "n": 40}],
                "patch_condition": ["n", "Lt", 3]}),
            json!({"durability": "applied",
                "patch_rows": [{"id": 1, "attributes": {"n": 40}},
                    {"id": 2, "attributes": {"n": 40}}],
                "patch_condition": ["n", "Lt", 3]}),
            (0, 1, 0),
            json!({}),
        ),
        (
            json!({"deletes": [1, 2], "delete_condition": ["n", "Eq", 1],
                "return_affected_ids": true}),
            json!({"durability": "applied", "affected_ids": true, "deletes": [1, 2],
                "delete_condition": ["n", "Eq", 1]}),
            (0, 0, 1),
            json!({"deleted_ids": [2]}),
        ),
        (
            json!({"upsert_rows": [{"id": 8, "vector": [0.5, 0.5], "n": 8}],
                "return_affected_ids": true, "distance_metric": "euclidean_squared"}),
            json!({"durability": "applied", "affected_ids": true,
                "distance_metric": "euclidean_squared",
                "documents": [{"id": 8, "vector": [0.5, 0.5], "attributes": {"n": 8}}]}),
            (1, 0, 0),
            json!({"upserted_ids": [8]}),
        ),
    ];
    for (i, (vendor, want_native, (up, patched, deleted), ids)) in cases.into_iter().enumerate() {
        let (f, n) = (format!("f{i}"), format!("n{i}"));
        seeded(&api, &f).await;
        seeded(&api, &n).await;
        let (s, fb) = call(
            &api,
            "POST",
            &format!("/v2/namespaces/{f}"),
            &key(7),
            Some(&vendor),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{vendor} -> {fb}");
        let (s, nb) = call(
            &api,
            "PUT",
            &format!("/v1/indexes/{n}/documents"),
            &native(7),
            Some(&want_native),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{want_native} -> {nb}");
        let a = &nb["applied"];
        assert_eq!(
            (
                &fb["rows_upserted"],
                &fb["rows_patched"],
                &fb["rows_deleted"]
            ),
            (&a["upserted"], &a["patched"], &a["deleted"]),
            "{vendor}: {fb} vs {nb}"
        );
        assert_eq!(
            (
                &fb["rows_upserted"],
                &fb["rows_patched"],
                &fb["rows_deleted"]
            ),
            (&json!(up), &json!(patched), &json!(deleted)),
            "{vendor}: {fb}"
        );
        assert_eq!(fb["rows_affected"], up + patched + deleted, "{fb}");
        // Only the non-empty lists, each the native outcome's.
        for k in ["upserted_ids", "patched_ids", "deleted_ids"] {
            assert_eq!(fb.get(k), ids.get(k), "{k}: {vendor} -> {fb}");
        }
        // The same rows remain, with the same values.
        assert_eq!(ns(&api, &f).await, ns(&api, &n).await, "{vendor}");
    }
    // `patch_by_filter` holds filters and patch only.
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/f0",
        &key(7),
        Some(&json!({"patch_by_filter": {"filters": ["n", "Eq", 1], "patch": {}, "x": 1}})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(b["error"].as_str().unwrap().contains("\"x\""), "{b}");
    // A server serving no applied writes refuses a conditional one, as natively.
    let plain = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    seeded(&plain, "docs").await;
    let (s, b) = call(
        &plain,
        "POST",
        "/v2/namespaces/docs",
        &key(7),
        Some(&json!({"delete_by_filter": ["kind", "Eq", "a"]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["error"]
            .as_str()
            .unwrap()
            .starts_with("unsupported_durability: "),
        "{b}"
    );
}

#[tokio::test]
async fn facade_multi_query_is_the_native_multi_query() {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    written(&api, 7).await;
    let (s, fb) = call(
        &api,
        "POST",
        "/v2/namespaces/docs/query",
        &key(7),
        Some(&json!({"queries": [
                {"rank_by": ["vector", "ANN", [1.0, 0.1]], "limit": 2, "include_attributes": ["n"]},
                {"rank_by": ["n", "asc"], "limit": 3, "include_attributes": ["n"],
                    "filters": ["kind", "Eq", "a"]},
                {"aggregate_by": {"c": ["Count"]}}
            ],
            "consistency": {"level": "eventual", "max_staleness_seconds": 5}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{fb}");
    let (s, nb) = call(
        &api,
        "POST",
        "/v1/indexes/docs/query",
        &native(7),
        Some(&json!({"queries": [
            {"vector": [1.0, 0.1], "top_k": 2, "include_attributes": ["n"],
                "consistency": "bounded", "max_staleness_ms": 5000},
            {"rank_by": ["n", "asc"], "top_k": 3, "include_attributes": ["n"],
                "filters": ["kind", "Eq", "a"], "consistency": "bounded",
                "max_staleness_ms": 5000},
            {"aggregate_by": {"c": ["Count"]}, "consistency": "bounded",
                "max_staleness_ms": 5000}
        ]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{nb}");
    let results = fb["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "{fb}");
    same_rows(&results[0], &json!({"results": nb["results"][0]}), "$dist");
    same_rows(&results[1], &json!({"results": nb["results"][1]}), "score");
    assert_eq!(
        results[2]["aggregations"], nb["aggregations"][2],
        "{fb} vs {nb}"
    );
    assert_eq!(results[2]["aggregations"]["c"], 4, "{fb}");
    assert!(results[0].get("aggregations").is_none(), "{fb}");
    assert!(fb["performance"]["server_total_ms"].is_u64(), "{fb}");
    // Refusals: what a later part translates, and a sub-query's own consistency.
    for (body, says) in [
        (
            json!({"queries": [{"rank_by": ["n", "asc"]}], "rerank_by": ["RRF"]}),
            "\"rerank_by\"",
        ),
        (
            json!({"queries": [{"rank_by": ["n", "asc"]}], "limit": 3}),
            "\"limit\"",
        ),
        (
            json!({"queries": [{"rank_by": ["n", "asc"], "consistency": {"level": "strong"}}]}),
            "root's",
        ),
        (json!({"queries": 3}), "an array of queries"),
    ] {
        let (s, b) = call(
            &api,
            "POST",
            "/v2/namespaces/docs/query",
            &key(7),
            Some(&body),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
        assert!(b["error"].as_str().unwrap().contains(says), "{body} -> {b}");
    }
}
