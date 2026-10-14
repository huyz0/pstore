//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M67: a `multi` leg's candidates from the fixed-dimensional encoding, through the API.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

const TENANT: u128 = 15;
const DIMS: usize = 128;
const PER: usize = 64;

async fn send<S: pstore_blob::BlobStore + Clone + 'static>(
    api: &Arc<Api<S>>,
    method: &str,
    uri: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT.to_string())
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn query<S: pstore_blob::BlobStore + Clone + 'static>(api: &Arc<Api<S>>, q: &Value) -> Value {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", Some(q)).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b
}

async fn refused<S: pstore_blob::BlobStore + Clone + 'static>(
    api: &Arc<Api<S>>,
    q: &Value,
) -> String {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", Some(q)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{q} -> {b}");
    b.to_string()
}

async fn fold<S: pstore_blob::BlobStore + Clone + 'static>(api: &Arc<Api<S>>) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn rnd(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

fn topic(t: usize) -> Vec<f32> {
    let mut s = 1000 + t as u64;
    (0..DIMS).map(|_| rnd(&mut s)).collect()
}

/// Document `i`: `PER` vectors around two topics, `i % 8` and `(i / 8) % 8`.
fn late(i: usize) -> Vec<Vec<f32>> {
    let (a, b) = (topic(i % 8), topic((i / 8) % 8));
    let mut s = 7 + i as u64;
    (0..PER)
        .map(|j| {
            let c = if j % 2 == 0 { &a } else { &b };
            c.iter().map(|x| x + 0.5 * rnd(&mut s)).collect()
        })
        .collect()
}

fn query_vectors(a: usize, b: usize) -> Vec<Vec<f32>> {
    let mut s = 31 + (a * 8 + b) as u64;
    let (ta, tb) = (topic(a), topic(b));
    (0..4)
        .map(|j| {
            let c = if j % 2 == 0 { &ta } else { &tb };
            c.iter().map(|x| x + 0.3 * rnd(&mut s)).collect()
        })
        .collect()
}

fn doc(i: usize) -> Value {
    json!({"id": format!("d{i:03}"), "vector": [1.0, 0.0, 0.0, 0.5],
           "vectors": {"late": late(i)}, "attributes": {"n": i as i64}})
}

async fn put<S: pstore_blob::BlobStore + Clone + 'static>(
    api: &Arc<Api<S>>,
    docs: std::ops::Range<usize>,
) {
    // A few documents a request: each carries 64 vectors of 128 floats as JSON.
    let all: Vec<usize> = docs.collect();
    for chunk in all.chunks(4) {
        let docs: Vec<Value> = chunk.iter().copied().map(doc).collect();
        let (s, b) = send(
            api,
            "PUT",
            "/v1/indexes/docs/documents",
            Some(&json!({"durability": "durable", "documents": docs,
                         "distance_metric": "dot_product"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
}

fn ids(b: &Value) -> Vec<String> {
    b["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results: {b}"))
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn compact(store: &MemoryStore) {
    let tenant = TenantId(TENANT);
    let engine = pstore_engine::Engine::new(
        Arc::new(Accounted::new(store.clone()).as_tenant(tenant)),
        tenant,
        LaneId(9),
    );
    assert!(engine.compact("docs").await.unwrap().is_some());
}

fn exact(a: usize, b: usize, k: usize) -> Value {
    json!({"multi": {"field": "late", "vectors": query_vectors(a, b)}, "top_k": k})
}

fn fde(a: usize, b: usize, k: usize, extra: Value) -> Value {
    let mut m = json!({"field": "late", "vectors": query_vectors(a, b), "mode": "fde"});
    for (key, v) in extra.as_object().unwrap() {
        m[key] = v.clone();
    }
    json!({"multi": m, "top_k": k})
}

#[tokio::test]
async fn fde_mode_ranks_and_exact_mode_is_unchanged() {
    let store = MemoryStore::new();
    let api = Api::new(Accounted::new(store.clone()), LaneId(1)).unwrap();
    // Two segments, then compacted into one, then a third: encodings survive each.
    put(&api, 0..32).await;
    fold(&api).await;
    put(&api, 32..64).await;
    fold(&api).await;
    compact(&store).await;
    put(&api, 64..80).await;
    fold(&api).await;
    for (a, b) in [(1usize, 2usize), (5, 0), (3, 3)] {
        let want = ids(&query(&api, &exact(a, b, 10)).await);
        assert_eq!(want.len(), 10);
        // The encoding approximates: the exact top ten are mostly within its top thirty of 80.
        let got = ids(&query(&api, &fde(a, b, 30, json!({}))).await);
        assert_eq!(got.len(), 30, "{a} {b}");
        let found = want.iter().filter(|w| got.contains(w)).count();
        assert!(
            found >= 7,
            "{a} {b}: only {found} of the exact top ten in {got:?}"
        );
    }
    // The exact leg is untouched by the mode being absent or `exact`.
    let plain = query(&api, &exact(1, 2, 10)).await;
    let mut named = exact(1, 2, 10);
    named["multi"]["mode"] = json!("exact");
    let named = query(&api, &named).await;
    assert_eq!(plain["results"], named["results"]);
}

#[tokio::test]
async fn rerank_exact_orders_by_maxsim() {
    let store = MemoryStore::new();
    let api = Api::new(Accounted::new(store.clone()), LaneId(1)).unwrap();
    put(&api, 0..40).await;
    fold(&api).await;
    put(&api, 40..80).await;
    fold(&api).await;
    let want = query(&api, &exact(2, 6, 10)).await;
    // A window covering the corpus: the exact order, scores included.
    let got = query(
        &api,
        &fde(2, 6, 10, json!({"rerank": "exact", "rerank_window": 80})),
    )
    .await;
    assert_eq!(ids(&got), ids(&want));
    for (g, w) in got["results"]
        .as_array()
        .unwrap()
        .iter()
        .zip(want["results"].as_array().unwrap())
    {
        let (g, w) = (g["score"].as_f64().unwrap(), w["score"].as_f64().unwrap());
        // Both are RRF scores of the same rank in one leg.
        assert!((g - w).abs() < 1e-6, "{g} vs {w}");
    }
    // A default window recovers the exact leader.
    let default = query(&api, &fde(2, 6, 10, json!({"rerank": "exact"}))).await;
    assert_eq!(ids(&default)[0], ids(&want)[0]);
}

#[tokio::test]
async fn fde_reads_less_than_the_exact_leg() {
    use pstore_testkit::depth::DepthCounting;
    let store = DepthCounting::new(MemoryStore::new());
    let cold = || Api::new(Accounted::new(store.clone()), LaneId(1)).unwrap();
    let writer = cold();
    for k in 0..2 {
        put(&writer, k * 20..k * 20 + 20).await;
        fold(&writer).await;
    }
    let mut seen = Vec::new();
    for body in [
        exact(1, 2, 10),
        fde(1, 2, 10, json!({})),
        fde(1, 2, 10, json!({"rerank": "exact", "rerank_window": 10})),
    ] {
        let api = cold();
        store.reset();
        let b = query(&api, &body).await;
        assert_eq!(ids(&b).len(), 10);
        seen.push((
            store.depth(),
            b["meta"]["cost"]["bytes_read"].as_u64().unwrap(),
        ));
    }
    let (exact_depth, exact_bytes) = seen[0];
    let (fde_depth, fde_bytes) = seen[1];
    assert_eq!(fde_depth, exact_depth, "{seen:?}");
    assert!(fde_bytes * 4 < exact_bytes, "{seen:?}");
    // The rerank is two reads after the leg's, and only the candidates' rows.
    let (rerank_depth, rerank_bytes) = seen[2];
    assert!(rerank_depth <= fde_depth + 2, "{seen:?}");
    assert!(rerank_bytes < exact_bytes, "{seen:?}");
}

#[tokio::test]
async fn the_derived_field_is_not_a_name_a_client_may_write() {
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    put(&api, 0..4).await;
    let mut d = doc(9);
    d["vectors"]["late$fde"] = json!([[1.0, 2.0]]);
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": [d]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(b.to_string().contains("$fde"), "{b}");
    // ...nor a field a query names.
    let why = refused(
        &api,
        &json!({"multi": {"field": "late$fde", "vectors": [[1.0, 2.0]]}}),
    )
    .await;
    assert!(why.contains("$fde"), "{why}");
}

#[tokio::test]
async fn fde_options_are_refused_at_the_door() {
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    put(&api, 0..8).await;
    fold(&api).await;
    let q = |extra: Value| fde(1, 2, 5, extra);
    let cases = [
        (q(json!({"mode": "approx"})), "mode"),
        (q(json!({"mode": 1})), "mode"),
        (q(json!({"rerank": "approx"})), "rerank"),
        (q(json!({"rerank": true})), "rerank"),
        (q(json!({"rerank_window": 50})), "rerank_window"),
        (
            q(json!({"rerank": "exact", "rerank_window": 4})),
            "rerank_window",
        ),
        (
            q(json!({"rerank": "exact", "rerank_window": 1001})),
            "rerank_window",
        ),
        (
            q(json!({"rerank": "exact", "rerank_window": 10.5})),
            "rerank_window",
        ),
    ];
    for (body, name) in cases {
        let why = refused(&api, &body).await;
        assert!(why.contains(name), "{body} -> {why}");
    }
    // M72: `probes` is a whole number from 1 to 1024, and only beside the `fde` mode.
    for bad in [json!(0), json!(1025), json!(2.5), json!("3"), json!(-1)] {
        let why = refused(&api, &fde(1, 2, 5, json!({"probes": bad.clone()}))).await;
        assert!(why.contains("probes"), "{bad} -> {why}");
    }
    let mut exact_probes = exact(1, 2, 5);
    exact_probes["multi"]["probes"] = json!(2);
    assert!(refused(&api, &exact_probes).await.contains("probes"));
    for ok in [1, 1024] {
        query(&api, &fde(1, 2, 5, json!({"probes": ok}))).await;
    }
    // `rerank` without the `fde` mode, and rerank beside a filter.
    let mut exact_rerank = exact(1, 2, 5);
    exact_rerank["multi"]["rerank"] = json!("exact");
    assert!(refused(&api, &exact_rerank).await.contains("rerank"));
    let mut filtered = fde(1, 2, 5, json!({"rerank": "exact"}));
    filtered["filters"] = json!(["n", "Gt", 2]);
    assert!(refused(&api, &filtered).await.contains("rerank"));
    // The limits are accepted, and a filter without rerank is served.
    query(
        &api,
        &fde(1, 2, 5, json!({"rerank": "exact", "rerank_window": 5})),
    )
    .await;
    query(
        &api,
        &fde(1, 2, 5, json!({"rerank": "exact", "rerank_window": 1000})),
    )
    .await;
    let mut f = fde(1, 2, 5, json!({}));
    f["filters"] = json!(["n", "Gt", 2]);
    assert!(!ids(&query(&api, &f).await).is_empty());
}
