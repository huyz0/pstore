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
use pstore_cache::CacheCore;
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
            json!({"upsert_rows": [], "sharding": {"num_shards": 2}}),
            "\"sharding\"",
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
    // Each side measured in a world of its own, built the same way, so neither sees what the
    // other wrote: requests grow with an index's history. A side is one call or, for
    // M270's update_metadata, the native calls it makes, in order.
    type Step<'a> = (&'a str, String, Vec<(&'static str, String)>, Value);
    // M271: a poll needs a copy started first, unmeasured; `{run}` in a path is its run.
    let cost_of = async |setup: &[Step<'_>], steps: &[Step<'_>]| {
        let depth = DepthCounting::new(MemoryStore::new());
        let store = Accounted::new(depth.clone());
        let api = cached_api(store.clone());
        let spent =
            || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(7), c));
        written(&api, 7).await;
        let mut run = String::new();
        for (m, p, h, b) in setup {
            let (s, body) = call(&api, m, p, h, Some(b)).await;
            assert!(s.is_success(), "{p}: {s} {body}");
            run = body["run"].to_string();
        }
        let before = spent();
        depth.reset();
        let mut statuses = Vec::new();
        for (m, p, h, b) in steps {
            let p = p.replace("{run}", &run);
            let (s, body) = call(&api, m, &p, h, Some(b)).await;
            assert!(
                s.is_success() || s == StatusCode::BAD_REQUEST,
                "{p}: {s} {body}"
            );
            statuses.push(s);
        }
        let after = spent();
        (
            before
                .iter()
                .zip(after)
                .map(|(b, a)| a - b)
                .collect::<Vec<_>>(),
            depth.depth(),
            statuses.last().map(|s| s.is_success()),
        )
    };
    let facade =
        |m, p: &str, b: Value| -> Step<'_> { (m, format!("/v2/namespaces/docs{p}"), key(7), b) };
    let v1 =
        |m, p: &str, b: Value| -> Step<'_> { (m, format!("/v1/namespaces/docs{p}"), key(7), b) };
    let native_step =
        |m, p: &str, b: Value| -> Step<'_> { (m, format!("/v1/indexes/docs{p}"), native(7), b) };
    let cases: Vec<(Step<'_>, Vec<Step<'_>>)> = vec![
        (
            facade(
                "POST",
                "/query",
                json!({"rank_by": ["vector", "ANN", [1.0, 0.1]], "limit": 3}),
            ),
            vec![native_step(
                "POST",
                "/query",
                json!({"vector": [1.0, 0.1], "top_k": 3, "consistency": "strong"}),
            )],
        ),
        (
            facade(
                "POST",
                "",
                json!({"upsert_rows": [{"id": 9, "vector": [1.0, 0.0], "n": 1}],
                    "distance_metric": "euclidean_squared"}),
            ),
            vec![native_step(
                "PUT",
                "/documents",
                json!({"durability": "durable", "distance_metric": "euclidean_squared",
                    "documents": [{"id": 9, "vector": [1.0, 0.0], "attributes": {"n": 1}}]}),
            )],
        ),
        // M269: a conditional write, applied, and a two-query multi-query.
        (
            facade(
                "POST",
                "",
                json!({"patch_rows": [{"id": 1, "n": 4}], "patch_condition": ["n", "Lt", 9],
                    "return_affected_ids": true}),
            ),
            vec![native_step(
                "PUT",
                "/documents",
                json!({"durability": "applied", "affected_ids": true,
                    "patch_rows": [{"id": 1, "attributes": {"n": 4}}],
                    "patch_condition": ["n", "Lt", 9]}),
            )],
        ),
        (
            facade(
                "POST",
                "/query",
                json!({"queries": [{"rank_by": ["vector", "ANN", [1.0, 0.1]], "limit": 3},
                    {"rank_by": ["n", "asc"], "limit": 2}]}),
            ),
            vec![native_step(
                "POST",
                "/query",
                json!({"queries": [{"vector": [1.0, 0.1], "top_k": 3, "consistency": "strong"},
                    {"rank_by": ["n", "asc"], "top_k": 2, "consistency": "strong"}]}),
            )],
        ),
        // M270: explain, warm, recall, the refused schema update, and update_metadata's two.
        (
            facade(
                "POST",
                "/explain_query",
                json!({"rank_by": ["vector", "ANN", [1.0, 0.1]], "limit": 3}),
            ),
            vec![native_step(
                "POST",
                "/query",
                json!({"vector": [1.0, 0.1], "top_k": 3, "consistency": "strong",
                    "explain": true}),
            )],
        ),
        (
            v1("GET", "/hint_cache_warm", json!({})),
            vec![native_step("POST", "/warm", json!({}))],
        ),
        (
            v1("POST", "/_debug/recall", json!({"num": 2, "top_k": 2})),
            vec![native_step(
                "POST",
                "/recall",
                json!({"num": 2, "top_k": 2}),
            )],
        ),
        (
            v1("POST", "/schema", json!({"schema": {"n": {"type": "int"}}})),
            vec![native_step("PATCH", "/schema", json!({}))],
        ),
        (
            v1("PATCH", "/metadata", json!({"read_only": true})),
            vec![
                native_step("PATCH", "", json!({"read_only": true})),
                native_step("GET", "", json!({})),
            ],
        ),
    ];
    let mut costs = Vec::new();
    for (f, n) in &cases {
        costs.push((
            f.1.clone(),
            cost_of(&[], std::slice::from_ref(f)).await,
            cost_of(&[], n).await,
        ));
    }
    // M271: a branch, an async copy's start, and its poll.
    let started = vec![native_step(
        "POST",
        "2/copy",
        json!({"source": {"index": "docs"}}),
    )];
    let copies: Vec<(Vec<Step<'_>>, Step<'_>, Vec<Step<'_>>)> = vec![
        (
            vec![],
            facade(
                "POST",
                "2?stainless_overload=branchFrom",
                json!({"branch_from_namespace": {"source_namespace": "docs"}}),
            ),
            vec![native_step(
                "PUT",
                "2/documents",
                json!({"branch_from_namespace": "docs"}),
            )],
        ),
        (
            vec![],
            facade(
                "POST",
                "2/async?stainless_overload=startCopyFrom",
                json!({"copy_from_namespace": {"source_namespace": "docs"}}),
            ),
            started.clone(),
        ),
        (
            started.clone(),
            v1("GET", "2/operations/{run}", json!({})),
            vec![native_step("GET", "2/copy", json!({}))],
        ),
    ];
    for (setup, f, n) in &copies {
        costs.push((
            f.1.clone(),
            cost_of(setup, std::slice::from_ref(f)).await,
            cost_of(setup, n).await,
        ));
    }
    for (path, f, n) in costs {
        assert_eq!(f, n, "{path}: facade then native");
    }
}

/// [`applied_api`] with a read cache, so a warm warms.
fn cached_api<S: BlobStore + 'static>(store: Accounted<S>) -> Arc<Api<S>> {
    let api =
        Api::with_cache_unsecured(store, LaneId(1), Arc::new(CacheCore::memory(16 << 20))).unwrap();
    api.set_applied_writes(true);
    api.set_affected_ids(true);
    api.set_idempotency(true);
    api.set_partial_filters(true);
    api.set_filter_ids(true);
    api
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

// M270: explain, warm, recall, schema and metadata updates.

/// The facade's error text, which names the native code.
fn refused(status: StatusCode, body: &Value, names: &str) {
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["status"], "error", "{body}");
    assert!(
        body["error"].as_str().unwrap().contains(names),
        "{names}: {body}"
    );
}

#[tokio::test]
async fn facade_explain_warm_and_recall_are_native_calls() {
    let api = cached_api(Accounted::new(MemoryStore::new()));
    seeded(&api, "docs").await;
    let v1 = |p: &str| format!("/v1/namespaces/docs{p}");

    // Delta 1: the plan is the native query's `meta.explain`, as text.
    for (f, n) in [(
        json!({"rank_by": ["vector", "ANN", [1.0, 0.1]], "limit": 3}),
        json!({"vector": [1.0, 0.1], "top_k": 3, "consistency": "strong", "explain": true}),
    )] {
        let (s, b) = call(
            &api,
            "POST",
            "/v2/namespaces/docs/explain_query",
            &key(7),
            Some(&f),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (s, native_b) =
            call(&api, "POST", "/v1/indexes/docs/query", &native(7), Some(&n)).await;
        assert_eq!(s, StatusCode::OK, "{native_b}");
        let plan = native_b
            .pointer("/meta/explain")
            .cloned()
            .unwrap_or(Value::Null);
        let text = &b["plan_text"];
        let got = text
            .as_str()
            .map_or(Value::Null, |t| serde_json::from_str(t).unwrap());
        assert_eq!(got, plan, "{b} vs {native_b}");
        assert_eq!(b.as_object().unwrap().len(), 1, "{b}");
    }
    // An order has no legs to explain: refused as the native explain refuses it.
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/docs/explain_query",
        &key(7),
        Some(&json!({"rank_by": ["n", "asc"], "limit": 2})),
    )
    .await;
    let (native_status, n) = call(
        &api,
        "POST",
        "/v1/indexes/docs/query",
        &native(7),
        Some(
            &json!({"rank_by": ["n", "asc"], "top_k": 2, "consistency": "strong",
            "explain": true}),
        ),
    )
    .await;
    assert_eq!(native_status, s, "{b} vs {n}");
    refused(s, &b, n["error"]["message"].as_str().unwrap());
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/docs/explain_query",
        &key(7),
        Some(&json!({"vector": [1.0, 0.1], "top_k": 3})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/docs/explain_query",
        &key(7),
        Some(&json!({"queries": [{"rank_by": ["n", "asc"], "limit": 1}]})),
    )
    .await;
    refused(s, &b, "explain_query explains one query");

    // Delta 2: the warm, ACCEPTED, naming what the native warm warmed.
    let (s, b) = call(&api, "GET", &v1("/hint_cache_warm"), &key(7), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, native_b) = call(&api, "POST", "/v1/indexes/docs/warm", &native(7), None).await;
    assert_eq!(s, StatusCode::OK, "{native_b}");
    assert_eq!(
        b,
        json!({"status": "ACCEPTED",
            "message": format!("warmed {} segments", native_b["segments"])}),
    );
    let (s, b) = call(
        &api,
        "GET",
        "/v1/namespaces/nothing/hint_cache_warm",
        &key(7),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    // On a server with no read cache there is nothing to warm, and that is no error.
    let plain = applied_api(Accounted::new(MemoryStore::new()));
    seeded(&plain, "docs").await;
    let (s, native_b) = call(&plain, "POST", "/v1/indexes/docs/warm", &native(7), None).await;
    assert_eq!(native_b["error"]["code"], "no_read_cache", "{s} {native_b}");
    let (s, b) = call(&plain, "GET", &v1("/hint_cache_warm"), &key(7), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "ACCEPTED", "{b}");
    assert!(
        b["message"].as_str().unwrap().contains("no read cache"),
        "{b}"
    );

    // Delta 3: recall, the native one's numbers, `dist` as `$dist`.
    for body in [
        json!({"num": 2, "top_k": 2, "include_ground_truth": true}),
        json!({"num": 1, "top_k": 3, "filters": ["kind", "Eq", "b"]}),
    ] {
        let (s, b) = call(&api, "POST", &v1("/_debug/recall"), &key(7), Some(&body)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (s, n) = call(
            &api,
            "POST",
            "/v1/indexes/docs/recall",
            &native(7),
            Some(&body),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{n}");
        for k in ["avg_recall", "avg_ann_count", "avg_exhaustive_count"] {
            assert_eq!(b[k], n[k], "{k}: {b} vs {n}");
        }
        for k in ["num", "epoch", "cost"] {
            assert!(b.get(k).is_none(), "{k} in {b}");
        }
        match n.get("ground_truth") {
            None => assert!(b.get("ground_truth").is_none(), "{b}"),
            Some(truth) => {
                let truth = truth.as_array().unwrap();
                let got = b["ground_truth"].as_array().unwrap();
                assert_eq!(got.len(), truth.len(), "{b}");
                assert!(!got.is_empty(), "{b}");
                for (g, t) in got.iter().zip(truth) {
                    assert_eq!(g["query_vector"], t["query_vector"], "{g} vs {t}");
                    let gn = g["nearest_neighbors"].as_array().unwrap();
                    let tn = t["nearest_neighbors"].as_array().unwrap();
                    assert_eq!(gn.len(), tn.len(), "{g} vs {t}");
                    for (a, z) in gn.iter().zip(tn) {
                        assert_eq!(
                            (&a["id"], &a["$dist"]),
                            (&z["id"], &z["dist"]),
                            "{a} vs {z}"
                        );
                        assert!(a.get("dist").is_none(), "{a}");
                    }
                }
            }
        }
    }
    let (s, b) = call(
        &api,
        "POST",
        &v1("/_debug/recall"),
        &key(7),
        Some(&json!({"num": 1, "rank_by": ["vector", "ANN", [1.0, 0.0]]})),
    )
    .await;
    refused(s, &b, "\"rank_by\" is not translated by this facade");
}

#[tokio::test]
async fn facade_schema_and_metadata_updates_are_native_calls() {
    let api = applied_api(Accounted::new(MemoryStore::new()));
    written(&api, 7).await;
    let meta = "/v1/namespaces/docs/metadata";

    // Delta 4: the native schema patch, refused as it refuses.
    let (s, b) = call(
        &api,
        "POST",
        "/v1/namespaces/docs/schema",
        &key(7),
        Some(&json!({"schema": {"n": {"type": "int", "filterable": true}}})),
    )
    .await;
    refused(s, &b, "schema_immutable");
    let (s, n) = call(
        &api,
        "PATCH",
        "/v1/indexes/docs/schema",
        &native(7),
        Some(&json!({})),
    )
    .await;
    assert_eq!(
        (s, &n["error"]["code"]),
        (StatusCode::BAD_REQUEST, &json!("schema_immutable"))
    );

    // Delta 6: the size is the native summary's.
    let (s, summary) = call(&api, "GET", "/v1/indexes/docs", &native(7), None).await;
    assert_eq!(s, StatusCode::OK, "{summary}");
    let (s, m) = call(&api, "GET", "/v2/namespaces/docs/metadata", &key(7), None).await;
    assert_eq!(s, StatusCode::OK, "{m}");
    assert!(
        summary["approx_logical_bytes"].as_u64().unwrap() > 0,
        "{summary}"
    );
    assert_eq!(
        m["approx_logical_bytes"], summary["approx_logical_bytes"],
        "{m}"
    );

    // Delta 5: read_only set, writes refused; cleared, writes in again.
    let (s, b) = call(
        &api,
        "PATCH",
        meta,
        &key(7),
        Some(&json!({"read_only": true})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, after) = call(&api, "GET", "/v2/namespaces/docs/metadata", &key(7), None).await;
    assert_eq!(s, StatusCode::OK, "{after}");
    assert_eq!(b, after);
    let (_, summary) = call(&api, "GET", "/v1/indexes/docs", &native(7), None).await;
    assert_eq!(summary["read_only"], true, "{summary}");
    let one = json!({"upsert_rows": [{"id": 8, "vector": [1.0, 0.0]}],
        "distance_metric": "euclidean_squared"});
    let (s, b) = call(&api, "POST", "/v2/namespaces/docs", &key(7), Some(&one)).await;
    refused(s, &b, "index_read_only");
    let (s, b) = call(
        &api,
        "PATCH",
        meta,
        &key(7),
        Some(&json!({"read_only": false, "pinning": null})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (_, summary) = call(&api, "GET", "/v1/indexes/docs", &native(7), None).await;
    assert_eq!(summary["read_only"], false, "{summary}");
    let (s, b) = call(&api, "POST", "/v2/namespaces/docs", &key(7), Some(&one)).await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // Asking for nothing answers the metadata; pinning is refused; nothing else is taken.
    let (s, b) = call(
        &api,
        "PATCH",
        meta,
        &key(7),
        Some(&json!({"pinning": false})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["approx_row_count"].is_u64(), "{b}");
    for (body, names) in [
        (json!({"pinning": true}), "pinning"),
        (json!({"pinning": {"replicas": 2}}), "pinning"),
        (json!({"read_only": "yes"}), "read_only"),
        (json!({"encryption": {}}), "encryption"),
    ] {
        let (s, b) = call(&api, "PATCH", meta, &key(7), Some(&body)).await;
        refused(s, &b, names);
    }
    let (s, b) = call(
        &api,
        "PATCH",
        "/v1/namespaces/nothing/metadata",
        &key(7),
        Some(&json!({"read_only": true})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}

// M271: copy, branch and the async copy.

#[tokio::test]
async fn facade_branch_and_copy_are_the_native_branch() {
    let api = applied_api(Accounted::new(MemoryStore::new()));
    seeded(&api, "src").await;
    let source = ns(&api, "src").await;
    assert_eq!(source.len(), 4, "{source:?}");
    // The native branch, the twin every form is held against.
    let (s, native_b) = call(
        &api,
        "PUT",
        "/v1/indexes/twin/documents",
        &native(7),
        Some(&json!({"branch_from_namespace": "src"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{native_b}");
    assert_eq!(ns(&api, "twin").await, source);
    let answer = json!({"status": "OK", "message": "0 rows affected", "rows_affected": 0,
        "rows_upserted": 0, "rows_patched": 0, "rows_deleted": 0});
    // As the SDK's branch_from, copy_from and write(copy_from_namespace=) send them.
    for (dest, query, body) in [
        (
            "b1",
            "?stainless_overload=branchFrom",
            json!({"branch_from_namespace": {"source_namespace": "src"}}),
        ),
        (
            "b2",
            "?stainless_overload=copyFrom",
            json!({"copy_from_namespace": {"source_namespace": "src"}}),
        ),
        ("b3", "", json!({"copy_from_namespace": "src"})),
        ("b4", "", json!({"branch_from_namespace": "src"})),
        (
            "b5",
            "",
            json!({"copy_from_namespace": {"source_namespace": "src"}}),
        ),
    ] {
        let path = format!("/v2/namespaces/{dest}{query}");
        let (s, b) = call(&api, "POST", &path, &key(7), Some(&body)).await;
        assert_eq!(s, StatusCode::OK, "{path}: {b}");
        assert_eq!(b, answer, "{path}");
        assert_eq!(ns(&api, dest).await, source, "{path}");
    }
    // A write to a copy is its own.
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/b2",
        &key(7),
        Some(&json!({"patch_rows": [{"id": 1, "n": 99}]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = call(&api, "POST", "/v1/admin/fold", &native(7), Some(&json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ns(&api, "src").await, source);
    assert_ne!(ns(&api, "b2").await, source);
    // Refusals: another organization, region or key; two sources; a source of the wrong
    // shape; and a branch beside a write, refused natively.
    for (path, body, names) in [
        (
            "/v2/namespaces/r1?stainless_overload=copyFrom",
            json!({"copy_from_namespace": {"source_namespace": "src", "source_region": "r"}}),
            "\"source_region\" is not served",
        ),
        (
            "/v2/namespaces/r1?stainless_overload=copyFrom",
            json!({"copy_from_namespace": {"source_namespace": "src", "source_api_key": "k"}}),
            "\"source_api_key\" is not served",
        ),
        (
            "/v2/namespaces/r1?stainless_overload=copyFrom",
            json!({"copy_from_namespace": {"source_namespace": "src", "dest_encryption": {}}}),
            "\"dest_encryption\" is not served",
        ),
        (
            "/v2/namespaces/r1",
            json!({"copy_from_namespace": "src", "branch_from_namespace": "src"}),
            "name one",
        ),
        (
            "/v2/namespaces/r1",
            json!({"copy_from_namespace": {"namespace": "src"}}),
            "source_namespace",
        ),
        (
            "/v2/namespaces/r1",
            json!({"source_namespace": "src"}),
            "\"source_namespace\" is not translated",
        ),
        (
            "/v2/namespaces/r1",
            json!({"copy_from_namespace": "src", "upsert_rows": [{"id": 9, "vector": [1.0, 0.0]}]}),
            "must come alone",
        ),
    ] {
        let (s, b) = call(&api, "POST", path, &key(7), Some(&body)).await;
        refused(s, &b, names);
    }
    let (s, b) = call(&api, "GET", "/v2/namespaces/r1/metadata", &key(7), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "nothing made: {b}");
}

/// Runs this process's replication worker until `dest`'s copy has ended.
async fn copied<S: BlobStore + 'static>(api: &Arc<Api<S>>, dest: &str) -> Value {
    let mut worker = pstore_server::Worker::default();
    for _ in 0..20 {
        api.replication_tick(&mut worker).await;
        let (s, b) = call(
            api,
            "GET",
            &format!("/v1/indexes/{dest}/copy"),
            &native(7),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        if !matches!(b["state"].as_str(), Some("running" | "paused")) {
            return b;
        }
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
    }
    panic!("{dest}'s copy did not end");
}

#[tokio::test(start_paused = true)]
async fn facade_async_copy_is_the_native_copy() {
    let api = applied_api(Accounted::new(MemoryStore::new()));
    seeded(&api, "src").await;
    let start =
        |dest: &str| format!("/v2/namespaces/{dest}/async?stainless_overload=startCopyFrom");
    let (s, b) = call(
        &api,
        "POST",
        &start("c1"),
        &key(7),
        Some(&json!({"copy_from_namespace": {"source_namespace": "src"}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let token = b["token"].as_str().unwrap().to_owned();
    assert_eq!(b.as_object().unwrap().len(), 1, "{b}");
    let (s, native_b) = call(&api, "GET", "/v1/indexes/c1/copy", &native(7), None).await;
    assert_eq!(s, StatusCode::OK, "{native_b}");
    assert_eq!(token, native_b["run"].to_string(), "{native_b}");
    let poll = |dest: &str, token: &str| format!("/v1/namespaces/{dest}/operations/{token}");

    // Running, as the native status says, before the worker has run.
    assert_eq!(native_b["state"], "running", "{native_b}");
    let (s, b) = call(&api, "GET", &poll("c1", &token), &key(7), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "running", "{b}");
    assert_eq!(b["start_time"], native_b["created_at"], "{b}");
    let (s, b) = call(&api, "GET", &poll("c1", "999999"), &key(7), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert!(
        b["error"]
            .as_str()
            .unwrap()
            .starts_with("operation_not_found"),
        "{b}"
    );
    let (s, b) = call(&api, "GET", &poll("none", &token), &key(7), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");

    // Finished, with the native times, and the copy reads the source's rows.
    let ended = copied(&api, "c1").await;
    assert_eq!(ended["state"], "succeeded", "{ended}");
    let (s, b) = call(&api, "GET", &poll("c1", &token), &key(7), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "finished", "{b}");
    assert_eq!(
        (&b["start_time"], &b["finish_time"]),
        (&ended["created_at"], &ended["ended_at"])
    );
    assert_eq!(b["result"]["success"]["status"], "OK", "{b}");
    assert_eq!(b["result"]["success"]["rows_affected"], 0, "{b}");
    assert_eq!(ns(&api, "c1").await, ns(&api, "src").await);

    // A missing source is refused before anything starts, as the native copy refuses it;
    // a run that fails later is `operation_answer`'s unit test.
    let (s, b) = call(
        &api,
        "POST",
        &start("c2"),
        &key(7),
        Some(&json!({"copy_from_namespace": {"source_namespace": "missing"}})),
    )
    .await;
    let (native_status, n) = call(
        &api,
        "POST",
        "/v1/indexes/c2/copy",
        &native(7),
        Some(&json!({"source": {"index": "missing"}})),
    )
    .await;
    assert_eq!(
        (s, native_status),
        (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND),
        "{b} vs {n}"
    );
    assert!(
        b["error"]
            .as_str()
            .unwrap()
            .starts_with("source_not_found: "),
        "{b}"
    );
    let (s, b) = call(
        &api,
        "POST",
        &start("c4"),
        &key(7),
        Some(&json!({"copy_from_namespace": {"source_namespace": "src", "source_api_key": "k"}})),
    )
    .await;
    refused(s, &b, "\"source_api_key\" is not served");
}

// M272: the compatibility manifest, held to the code.

const MANIFEST: &str = include_str!("../../../../api/compat/turbopuffer.json");

fn manifest() -> Value {
    serde_json::from_str(MANIFEST).unwrap()
}

#[test]
fn the_manifest_is_the_facades_routes() {
    // Every `.facade(` registration in the router: its path and each method on it.
    let lib = include_str!("../../src/lib.rs");
    let path = regex::Regex::new(r#"^\s*"(/v[12]/[^"]*)""#).unwrap();
    let method = regex::Regex::new(r"(get|post|patch|put|delete)\(facade::").unwrap();
    let mut registered = std::collections::BTreeSet::new();
    for block in lib.split(".facade(").skip(1) {
        let block = block.split(".route(").next().unwrap();
        let block = block.split("\n    }").next().unwrap();
        let p = block
            .lines()
            .find_map(|l| path.captures(l))
            .unwrap_or_else(|| panic!("a facade route with no path: {block}"))[1]
            .replace("{index}", "{namespace}");
        for m in method.captures_iter(block) {
            registered.insert(format!("{} {p}", m[1].to_uppercase()));
        }
    }
    assert!(registered.len() >= 13, "{registered:?}");
    let listed: std::collections::BTreeSet<String> = manifest()["operations"]
        .as_object()
        .unwrap()
        .values()
        .map(|o| o["route"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(registered, listed, "registered, then the manifest's");
}

#[test]
fn the_manifest_evidence_resolves() {
    let sdk = include_str!("../../../../clients/turbopuffer/test_sdk.py");
    let rust = [
        include_str!("facade.rs"),
        include_str!("../../src/facade.rs"),
    ];
    let m = manifest();
    for (name, op) in m["operations"].as_object().unwrap() {
        let status = op["status"].as_str().unwrap();
        assert!(matches!(status, "served" | "refused"), "{name}: {status}");
        let evidence = op["evidence"].as_array().unwrap();
        assert!(!evidence.is_empty(), "{name} has no evidence");
        for e in evidence {
            let e = e.as_str().unwrap();
            let found = match e.split_once(':') {
                Some(("sdk", t)) => sdk.contains(&format!("def {t}(")),
                Some(("rust", t)) => rust.iter().any(|r| r.contains(&format!("fn {t}("))),
                _ => false,
            };
            assert!(found, "{name}: {e} names no test");
        }
    }
}

// M274: a resent conditional or by-filter write applies at most once.

/// [`key`], with the SDK's attempt number.
fn attempt(tenant: u128, n: u32) -> Vec<(&'static str, String)> {
    let mut h = key(tenant);
    h.push(("x-stainless-retry-count", n.to_string()));
    h
}

/// A native durable write of `body` into `index`, folded: another writer.
async fn natively<S: BlobStore + 'static>(api: &Arc<Api<S>>, index: &str, body: Value) {
    let (s, b) = call(
        api,
        "PUT",
        &format!("/v1/indexes/{index}/documents"),
        &native(7),
        Some(&body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = call(api, "POST", "/v1/admin/fold", &native(7), Some(&json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn n_of(rows: &[(Value, Value)], id: u64) -> Value {
    rows.iter()
        .find(|(i, _)| *i == json!(id))
        .map_or(Value::Null, |(_, n)| n.clone())
}

#[tokio::test]
async fn a_resent_conditional_write_applies_once() {
    let api = applied_api(Accounted::new(MemoryStore::new()));
    seeded(&api, "docs").await;
    let body = json!({"patch_rows": [{"id": 2, "n": 7}], "patch_condition": ["n", "Eq", 1]});
    let (s, first) = call(
        &api,
        "POST",
        "/v2/namespaces/docs",
        &attempt(7, 0),
        Some(&body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{first}");
    assert_eq!(first["rows_patched"], 1, "{first}");
    assert_eq!(n_of(&ns(&api, "docs").await, 2), json!(7));
    // Another writer puts the condition back; a second evaluation would apply again.
    natively(
        &api,
        "docs",
        json!({"durability": "durable", "patch_rows": [{"id": 2, "attributes": {"n": 1}}]}),
    )
    .await;
    // The SDK's resend of the same body, its first answer lost.
    let (s, again) = call(
        &api,
        "POST",
        "/v2/namespaces/docs",
        &attempt(7, 1),
        Some(&body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(
        again["rows_patched"], 1,
        "the first attempt's counts: {again}"
    );
    assert_eq!(
        n_of(&ns(&api, "docs").await, 2),
        json!(1),
        "not applied again"
    );
}

#[tokio::test]
async fn a_resent_by_filter_delete_applies_once() {
    let api = applied_api(Accounted::new(MemoryStore::new()));
    seeded(&api, "docs").await;
    let body = json!({"delete_by_filter": ["kind", "Eq", "a"]});
    let (s, first) = call(
        &api,
        "POST",
        "/v2/namespaces/docs",
        &attempt(7, 0),
        Some(&body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{first}");
    assert_eq!(first["rows_deleted"], 2, "{first}");
    natively(
        &api,
        "docs",
        json!({"durability": "durable", "distance_metric": "euclidean_squared",
            "documents": [{"id": 9, "vector": [1.0, 0.0], "attributes": {"n": 9, "kind": "a"}}]}),
    )
    .await;
    for n in [1, 2] {
        let (s, again) = call(
            &api,
            "POST",
            "/v2/namespaces/docs",
            &attempt(7, n),
            Some(&body),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{again}");
        assert_eq!(again["rows_deleted"], 2, "{again}");
    }
    assert_eq!(n_of(&ns(&api, "docs").await, 9), json!(9), "row 9 kept");
}

#[tokio::test]
async fn an_unmatched_resend_is_refused() {
    let api = applied_api(Accounted::new(MemoryStore::new()));
    seeded(&api, "docs").await;
    let before = ns(&api, "docs").await;
    let body = json!({"patch_rows": [{"id": 2, "n": 7}], "patch_condition": ["n", "Eq", 1]});
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/docs",
        &attempt(7, 1),
        Some(&body),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["error"]
            .as_str()
            .unwrap()
            .starts_with("retry_outcome_unknown: "),
        "{b}"
    );
    assert_eq!(ns(&api, "docs").await, before, "nothing applied");
    // Another tenant's first attempt of the same bytes is not this tenant's.
    call(
        &api,
        "POST",
        "/v2/namespaces/docs",
        &attempt(8, 0),
        Some(&body),
    )
    .await;
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/docs",
        &attempt(7, 1),
        Some(&body),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    // A first attempt is unchanged.
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/docs",
        &attempt(7, 0),
        Some(&body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["rows_patched"], 1, "{b}");
}

#[tokio::test]
async fn a_resend_without_keys_is_refused() {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    api.set_applied_writes(true);
    seeded(&api, "docs").await;
    let body = json!({"patch_rows": [{"id": 2, "n": 7}], "patch_condition": ["n", "Eq", 1]});
    // A first attempt, unkeyed as before M274.
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/docs",
        &attempt(7, 0),
        Some(&body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["rows_patched"], 1, "{b}");
    let before = ns(&api, "docs").await;
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/docs",
        &attempt(7, 1),
        Some(&body),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["error"]
            .as_str()
            .unwrap()
            .starts_with("retry_outcome_unknown: "),
        "{b}"
    );
    assert_eq!(ns(&api, "docs").await, before);
}

#[tokio::test]
async fn a_plain_write_is_not_keyed() {
    let api = applied_api(Accounted::new(MemoryStore::new()));
    seeded(&api, "docs").await;
    // Resends this server never saw: no condition, no by-filter operation, so not refused.
    for body in [
        json!({"upsert_rows": [{"id": 1, "vector": [1.0, 0.0], "n": 11}],
            "distance_metric": "euclidean_squared"}),
        json!({"upsert_rows": [{"id": 2, "vector": [1.0, 0.0], "n": 12}],
            "distance_metric": "euclidean_squared", "return_affected_ids": true}),
        json!({"deletes": [3]}),
    ] {
        let (s, b) = call(
            &api,
            "POST",
            "/v2/namespaces/docs",
            &attempt(7, 2),
            Some(&body),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body} -> {b}");
    }
}

#[tokio::test]
async fn a_keyed_facade_write_costs_the_native_pair() {
    let body = json!({"patch_rows": [{"id": 2, "n": 7}], "patch_condition": ["n", "Eq", 1]});
    let measured = async |facade: bool| {
        let depth = DepthCounting::new(MemoryStore::new());
        let store = Accounted::new(depth.clone());
        let api = applied_api(store.clone());
        seeded(&api, "docs").await;
        let spent =
            || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(7), c));
        let before = spent();
        depth.reset();
        if facade {
            let (s, b) = call(
                &api,
                "POST",
                "/v2/namespaces/docs",
                &attempt(7, 0),
                Some(&body),
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{b}");
        } else {
            let (s, k) = call(
                &api,
                "POST",
                "/v1/indexes/docs/idempotency-keys",
                &native(7),
                Some(&json!({})),
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{k}");
            let (s, b) = call(
                &api,
                "PUT",
                "/v1/indexes/docs/documents",
                &native(7),
                Some(&json!({"durability": "applied",
                    "idempotency_key": k["idempotency_key"],
                    "patch_rows": [{"id": 2, "attributes": {"n": 7}}],
                    "patch_condition": ["n", "Eq", 1]})),
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{b}");
        }
        let after = spent();
        let used: Vec<_> = before.iter().zip(after).map(|(b, a)| a - b).collect();
        (used, depth.depth())
    };
    assert_eq!(
        measured(true).await,
        measured(false).await,
        "facade, then native"
    );
}

// M275: the facade's schema in the vendor's names, both ways.

#[tokio::test]
async fn facade_schema_lists_every_recorded_declaration() {
    let api = applied_api(Accounted::new(MemoryStore::new()));
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/typed",
        &key(7),
        Some(&json!({
            "upsert_rows": [{"id": 1, "vector": [1.0, 0.0], "title": "red fox",
                "body": "a quick note", "path": "/a/b",
                "at": "2026-11-17T00:00:00Z", "big": 18446744073709551615u64,
                "seen": ["2026-11-17T00:00:00Z"], "n": 3}],
            "distance_metric": "cosine_distance",
            "schema": {
                "id": "uint",
                "vector": {"type": "[2]f16", "ann": true},
                "title": {"type": "string", "full_text_search": true},
                "body": {"type": "string", "full_text_search": {"stemming": true}},
                "path": {"type": "string", "regex": true},
                "at": "datetime",
                "big": {"type": "uint"},
                "seen": "[]datetime"
            }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = call(&api, "POST", "/v1/admin/fold", &native(7), Some(&json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, summary) = call(&api, "GET", "/v1/indexes/typed", &native(7), None).await;
    assert_eq!(s, StatusCode::OK, "{summary}");
    let sc = &summary["schema"];
    let (s, schema) = call(&api, "GET", "/v1/namespaces/typed/schema", &key(7), None).await;
    assert_eq!(s, StatusCode::OK, "{schema}");
    let ann = json!({"distance_metric": "cosine_distance"});
    assert_eq!(
        schema,
        json!({
            "id": {"type": "uint"},
            "vector": {"type": "[2]f16", "ann": ann},
            "title": {"full_text_search": sc["full_text_search"]},
            "body": {"full_text_search": sc["text_fields"]["body"]},
            "path": {"regex": true},
            "at": {"type": "datetime"},
            "big": {"type": "uint"},
            "seen": {"type": "datetime"}
        }),
        "{summary}"
    );
    assert_eq!(sc["full_text_search"]["stemming"], false, "{sc}");
    assert_eq!(sc["text_fields"]["body"]["stemming"], true, "{sc}");
    // metadata() answers the same schema.
    let (s, meta) = call(&api, "GET", "/v2/namespaces/typed/metadata", &key(7), None).await;
    assert_eq!(s, StatusCode::OK, "{meta}");
    assert_eq!(meta["schema"], schema);
    // A named field, f32: its width and the index's metric.
    let (s, b) = call(
        &api,
        "POST",
        "/v2/namespaces/named",
        &key(7),
        Some(
            &json!({"upsert_rows": [{"id": "a", "vector": [1.0, 0.0], "text": "red fox",
            "vectors": {"img": [[0.5, 0.5, 0.5]]}}], "distance_metric": "euclidean_squared"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    call(&api, "POST", "/v1/admin/fold", &native(7), Some(&json!({}))).await;
    let (_, summary) = call(&api, "GET", "/v1/indexes/named", &native(7), None).await;
    let sc = &summary["schema"];
    // The document's `text` is the index's primary text field, not a further one.
    let text = sc["text_field"].as_str().unwrap();
    assert!(!text.is_empty(), "{sc}");
    let (s, schema) = call(&api, "GET", "/v1/namespaces/named/schema", &key(7), None).await;
    assert_eq!(s, StatusCode::OK, "{schema}");
    let ann = json!({"distance_metric": "euclidean_squared"});
    assert_eq!(
        schema,
        json!({"id": {"type": "string"}, "vector": {"type": "[2]f32", "ann": ann},
            "img": {"type": "[3]f32", "ann": ann},
            text: {"full_text_search": sc["full_text_search"]}})
    );
}
