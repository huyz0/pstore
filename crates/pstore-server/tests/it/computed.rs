//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `compute_attributes` (M265): per-row expression values and vector distances, which never
//! change the ranking.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_server::Api;
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type Store = Accounted<DepthCounting<MemoryStore>>;
type A = Arc<Api<DepthCounting<MemoryStore>>>;

const TENANT: u128 = 51;

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT.to_string())
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(r).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn query(api: &A, q: &Value) -> Value {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", q).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b
}

const WORDS: [&str; 6] = ["fox", "dog", "cat", "owl", "bee", "elk"];

fn vector(i: usize) -> Vec<f32> {
    let x = i as f32;
    vec![x.sin(), x.cos(), (x * 0.3).sin()]
}

fn title(i: usize) -> Vec<f32> {
    let x = i as f32;
    vec![(x * 0.7).cos(), (x * 0.2).sin()]
}

/// Row `i`: a text of repeated words, a body, a number, and on even rows a `title` vector.
fn doc(i: usize) -> Value {
    let text: Vec<&str> = (0..=(i % 5))
        .map(|k| WORDS[(i * 7 + k * 3) % WORDS.len()])
        .collect();
    let mut d = json!({"id": format!("d{i:03}"), "vector": vector(i), "text": text.join(" "),
        "attributes": {"n": i, "body": WORDS[i % WORDS.len()]}});
    if i.is_multiple_of(2) {
        d["vectors"] = json!({"title": [title(i)]});
    }
    d
}

async fn write(api: &A, range: std::ops::Range<usize>, durable: bool, metric: &str) {
    let documents: Vec<Value> = range.map(doc).collect();
    let body = json!({"durability": if durable { "durable" } else { "batched" },
        "distance_metric": metric, "documents": documents,
        "schema": {"text": {"type": "string", "full_text_search": true},
                   "body": {"type": "string", "full_text_search": {"k1": 1.5}}}});
    let (s, b) = send(api, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// 60 rows: two folded segments and 20 unfolded rows.
async fn corpus(metric: &str) -> (A, Store) {
    corpus_counted(metric).await.0
}

/// [`corpus`], and the depth counter beneath its accounting.
async fn corpus_counted(metric: &str) -> ((A, Store), DepthCounting<MemoryStore>) {
    let depth = DepthCounting::new(MemoryStore::new());
    let store = Accounted::new(depth.clone());
    let api = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    write(&api, 0..20, true, metric).await;
    fold(&api).await;
    write(&api, 20..40, true, metric).await;
    fold(&api).await;
    write(&api, 40..60, false, metric).await;
    ((api, store), depth)
}

fn expression() -> Value {
    json!([
        "Sum",
        [
            ["text", "BM25", "fox fox dog"],
            ["Product", 2.0, ["body", "BM25", "cat"]],
            ["n", "Gte", 30]
        ]
    ])
}

#[tokio::test]
async fn computed_bm25_equals_rank_by() {
    let (api, _) = corpus("cosine_distance").await;
    let e = expression();
    let scored = query(&api, &json!({"rank_by": e, "top_k": 100})).await;
    let by_rank: BTreeMap<String, f64> = scored["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                r["score"].as_f64().unwrap_or_else(|| panic!("{r}")),
            )
        })
        .collect();
    assert!(by_rank.len() > 10, "{scored}");
    for q in [
        json!({"vector": [0.2, 0.9, 0.1], "top_k": 60}),
        json!({"text": "owl bee", "top_k": 60}),
    ] {
        let mut q = q;
        q["compute_attributes"] = json!({"s": e, "only_owl": ["text", "BM25", "owl"],
            "n": ["Attribute", "n"], "none": ["Attribute", "missing"]});
        let got = query(&api, &q).await;
        let rows = got["results"].as_array().unwrap();
        assert!(!rows.is_empty(), "{got}");
        let mut matched = 0;
        for r in rows {
            let id = r["id"].as_str().unwrap();
            let s = r["$computed"]["s"].as_f64().unwrap();
            match by_rank.get(id) {
                Some(want) => {
                    assert_eq!(s, *want, "{id}: {r}");
                    matched += 1;
                }
                None => assert_eq!(s, 0.0, "{id}: {r}"),
            }
            assert!(r["$computed"]["only_owl"].as_f64().is_some(), "{r}");
            // A root `Attribute` is the row's value, and null where it has none.
            let i: f64 = id[1..].parse().unwrap();
            assert_eq!(r["$computed"]["n"], i, "{r}");
            assert_eq!(r["$computed"]["none"], Value::Null, "{r}");
        }
        assert!(matched > 5, "{got}");
    }
}

#[tokio::test]
async fn computed_attributes_never_change_the_page() {
    let (api, _) = corpus("dot_product").await;
    for q in [
        json!({"vector": [0.2, 0.9, 0.1], "top_k": 15, "include_attributes": ["n"]}),
        json!({"text": "fox owl", "top_k": 15}),
        json!({"vector": [0.5, 0.1, 0.4], "text": "cat", "top_k": 15}),
    ] {
        let plain = query(&api, &q).await;
        let mut with = q.clone();
        with["compute_attributes"] = json!({"s": expression(),
            "d": ["vector", "VectorDist", [1.0, 0.0, 0.0]]});
        let got = query(&api, &with).await;
        let strip = |b: &Value| {
            let mut rows = b["results"].clone();
            for r in rows.as_array_mut().unwrap() {
                r.as_object_mut().unwrap().remove("$computed");
            }
            rows
        };
        assert_eq!(strip(&got), plain["results"], "{q}");
        assert!(plain["results"][0].get("$computed").is_none(), "{plain}");
        assert!(
            got["results"][0].get("vector").is_none(),
            "not projected: {got}"
        );
    }
}

fn metric_dist(metric: &str, q: &[f32], v: &[f32]) -> f64 {
    let dot: f64 = q
        .iter()
        .zip(v)
        .map(|(a, b)| f64::from(*a) * f64::from(*b))
        .sum();
    match metric {
        "dot_product" => -dot,
        "cosine_distance" => {
            let n = |x: &[f32]| x.iter().map(|a| f64::from(*a).powi(2)).sum::<f64>().sqrt();
            1.0 - dot / (n(q) * n(v))
        }
        _ => q
            .iter()
            .zip(v)
            .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
            .sum(),
    }
}

fn close(got: f64, want: f64, what: &str) {
    assert!(
        (got - want).abs() <= want.abs() * 1e-5 + 1e-5,
        "{what}: {got} != {want}"
    );
}

#[tokio::test]
async fn computed_vector_dist() {
    for metric in ["dot_product", "cosine_distance", "euclidean_squared"] {
        let (api, _) = corpus(metric).await;
        let q = [0.2_f32, 0.9, 0.1];
        let other = [0.0_f32, 1.0, -1.0];
        let t = [0.6_f32, -0.8];
        let got = query(
            &api,
            &json!({"vector": q, "top_k": 30, "exact": true, "compute_attributes": {
                "same": ["vector", "VectorDist", q],
                "other": ["vector", "VectorDist", other],
                "title": ["title", "VectorDist", t]}}),
        )
        .await;
        let rows = got["results"].as_array().unwrap();
        assert_eq!(rows.len(), 30, "{got}");
        for r in rows {
            let id = r["id"].as_str().unwrap();
            let i: usize = id[1..].parse().unwrap();
            let c = &r["$computed"];
            close(
                c["same"].as_f64().unwrap(),
                r["$dist"].as_f64().unwrap(),
                &format!("{metric} {id} same"),
            );
            close(
                c["other"].as_f64().unwrap(),
                metric_dist(metric, &other, &vector(i)),
                &format!("{metric} {id} other"),
            );
            if i.is_multiple_of(2) {
                close(
                    c["title"].as_f64().unwrap(),
                    metric_dist(metric, &t, &title(i)),
                    &format!("{metric} {id} title"),
                );
            } else {
                assert_eq!(c["title"], Value::Null, "{metric} {id}: {r}");
            }
        }
        // A vector of another width is refused.
        let (s, b) = send(
            &api,
            "POST",
            "/v1/indexes/docs/query",
            &json!({"vector": q, "top_k": 5,
                    "compute_attributes": {"d": ["vector", "VectorDist", [1.0, 0.0]]}}),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert!(b.to_string().contains("holds [3] vectors"), "{b}");
    }
}

#[tokio::test]
async fn computed_attributes_add_no_round() {
    let ((api, _), counter) = corpus_counted("cosine_distance").await;
    let depth = |q: Value| {
        let api = Arc::clone(&api);
        let counter = counter.clone();
        async move {
            counter.reset();
            query(&api, &q).await;
            counter.depth()
        }
    };
    for q in [
        json!({"vector": [0.2, 0.9, 0.1], "top_k": 10}),
        json!({"text": "fox owl", "top_k": 10}),
    ] {
        let plain = depth(q.clone()).await;
        let mut with = q.clone();
        with["compute_attributes"] = json!({"s": expression(),
            "d": ["vector", "VectorDist", [1.0, 0.0, 0.0]]});
        let computed = depth(with).await;
        assert_eq!(computed, plain, "{q}");
        assert!(plain >= 2, "{q}: depth {plain}");
    }
}

#[tokio::test]
async fn computed_attributes_refusals() {
    let (api, store) = corpus("dot_product").await;
    let spent =
        || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(TENANT), c));
    let v = json!([0.2, 0.9, 0.1]);
    let many: serde_json::Map<String, Value> = (0..257)
        .map(|i| (format!("a{i}"), json!(["n", "Gte", i])))
        .collect();
    for (q, says) in [
        (
            json!({"rank_by": ["n", "asc"], "compute_attributes": {"a": ["n", "Gte", 1]}}),
            "an order, a scoring rank_by or an aggregation",
        ),
        (
            json!({"rank_by": expression(), "compute_attributes": {"a": ["n", "Gte", 1]}}),
            "an order, a scoring rank_by or an aggregation",
        ),
        (
            json!({"aggregate_by": {"c": ["Count"]}, "compute_attributes": {"a": ["n", "Gte", 1]}}),
            "an order, a scoring rank_by or an aggregation",
        ),
        (
            json!({"vector": v, "as_of": 1, "compute_attributes": {"a": ["n", "Gte", 1]}}),
            "as_of",
        ),
        (
            json!({"vector": v, "compute_attributes": Value::Object(many)}),
            "1 to 256 names",
        ),
        (
            json!({"vector": v, "compute_attributes": {}}),
            "1 to 256 names",
        ),
        (
            json!({"vector": v, "compute_attributes": {"": ["n", "Gte", 1]}}),
            "is not a name",
        ),
        (
            json!({"vector": v, "compute_attributes": {"$x": ["n", "Gte", 1]}}),
            "is not a name",
        ),
        (
            json!({"vector": v, "compute_attributes": {"a": ["Sum", []]}}),
            "compute_attributes: a: ",
        ),
        (
            json!({"vector": v, "compute_attributes": {"a": ["sparse", "VectorDist", [1.0]]}}),
            "is sparse",
        ),
        (
            json!({"vector": v, "compute_attributes": {"a": ["vector", "VectorDist", []]}}),
            "non-empty array of numbers",
        ),
        (json!({"vector": v, "compute_attributes": [1]}), "an object"),
    ] {
        let before = spent();
        let (s, b) = send(&api, "POST", "/v1/indexes/docs/query", &q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{q} -> {b}");
        assert!(b.to_string().contains(says), "{q} -> {b}");
        assert_eq!(spent(), before, "{q}: a refusal reads nothing");
    }
    // A BM25 clause over a field that is not full-text: refused by the engine, from HEAD.
    let (s, b) = send(
        &api,
        "POST",
        "/v1/indexes/docs/query",
        &json!({"vector": v, "compute_attributes": {"a": ["nope", "BM25", "x"]}}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}
