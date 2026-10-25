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

//! M64: a ranked window diversified by maximal marginal relevance.
//!
//! The model is the test's own: the same query unboosted at `top_k = window`, whose scores and
//! the vectors the test wrote feed a greedy MMR written here in `f64`. A guard asserts every
//! choice's margin over the runner-up is wide enough that rounding cannot reorder it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

async fn send<S: BlobStore + Clone + 'static>(
    api: &Arc<Api<S>>,
    method: &str,
    uri: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "41")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn api() -> Arc<Api<MemoryStore>> {
    Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap()
}

async fn query<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>, q: &Value) -> Value {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", Some(q)).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b
}

async fn fold<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Three clusters of near-duplicates: row `i` is in cluster `i % 3`, nudged by a distinct,
/// deterministic amount, so no two rows tie.
fn vector(i: usize) -> Vec<f32> {
    let centers = [
        [1.0, 0.0, 0.0, 0.2],
        [0.0, 1.0, 0.0, 0.2],
        [0.0, 0.0, 1.0, 0.2],
    ];
    let c = centers[i % 3];
    // Unequal norms, so dot product and euclidean rank by magnitude and cosine does not.
    let scale = 1.0 + 0.7 * (i % 4) as f32;
    (0..4)
        .map(|j| scale * (c[j] + 0.04 * ((i * 7 + j * 3 + 1) as f32).sin()))
        .collect()
}

const Q: [f32; 4] = [1.0, 0.8, 0.1, 0.2];

async fn write<S: BlobStore + Clone + 'static>(
    api: &Arc<Api<S>>,
    metric: &str,
    rows: std::ops::Range<usize>,
) {
    let docs: Vec<Value> = rows
        .map(|i| {
            json!({"id": format!("d{i:02}"), "vector": vector(i),
                   "attributes": {"tag": (i % 5) as i64}})
        })
        .collect();
    let (s, b) = send(
        api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": docs, "distance_metric": metric})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// A candidate: its id, the score the engine gave it, and the vector the test wrote.
struct Cand {
    id: String,
    score: f64,
    v: Vec<f32>,
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        na += f64::from(*x) * f64::from(*x);
        nb += f64::from(*y) * f64::from(*y);
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Greedy MMR, as the spec states it, and the narrowest margin any choice won by.
fn mmr(cands: &[Cand], lambda: f64, k: usize) -> (Vec<String>, f64) {
    let (lo, hi) = cands
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), c| {
            (lo.min(c.score), hi.max(c.score))
        });
    let rel = |c: &Cand| {
        if hi == lo {
            1.0
        } else {
            (c.score - lo) / (hi - lo)
        }
    };
    let mut picked: Vec<usize> = Vec::new();
    let mut narrowest = f64::INFINITY;
    while picked.len() < k.min(cands.len()) {
        let mut best = (f64::NEG_INFINITY, usize::MAX);
        let mut second = f64::NEG_INFINITY;
        for (d, c) in cands
            .iter()
            .enumerate()
            .filter(|(d, _)| !picked.contains(d))
        {
            let penalty = picked
                .iter()
                .map(|p| cosine(&c.v, &cands[*p].v))
                .fold(None, |m: Option<f64>, s| Some(m.map_or(s, |m| m.max(s))))
                .unwrap_or(0.0);
            let obj = lambda * rel(c) - (1.0 - lambda) * penalty;
            if obj > best.0 {
                second = best.0;
                best = (obj, d);
            } else if obj > second {
                second = obj;
            }
        }
        let (b, d) = best;
        // At lambda 0 the first pick ties every candidate at 0, and the earliest wins.
        if !(lambda == 0.0 && picked.is_empty()) && second.is_finite() {
            narrowest = narrowest.min(b - second);
        }
        picked.push(d);
    }
    (
        picked.iter().map(|d| cands[*d].id.clone()).collect(),
        narrowest,
    )
}

/// The candidates of `q` unboosted at `top_k = window`, in the engine's order.
async fn candidates<S: BlobStore + Clone + 'static>(
    api: &Arc<Api<S>>,
    q: &Value,
    window: usize,
) -> Vec<Cand> {
    let mut plain = q.clone();
    plain["top_k"] = json!(window);
    let b = query(api, &plain).await;
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let id = r["id"].as_str().unwrap().to_owned();
            let i: usize = id[1..].parse().unwrap();
            Cand {
                id,
                score: r["score"].as_f64().unwrap(),
                v: vector(i),
            }
        })
        .collect()
}

fn ids(b: &Value) -> Vec<String> {
    b["results"]
        .as_array()
        .unwrap_or_else(|| panic!("{b}"))
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn mmr_picks_as_the_model_does() {
    for metric in ["cosine_distance", "euclidean_squared", "dot_product"] {
        let a = api();
        write(&a, metric, 0..30).await;
        let mut docs = 30;
        for when in ["unfolded", "folded", "mixed"] {
            if when == "mixed" {
                // Six more rows, unfolded, beside the folded thirty.
                write(&a, metric, 30..36).await;
                docs = 36;
            }
            let base = json!({"vector": Q, "exact": true, "top_k": 6});
            let cands = candidates(&a, &base, 40).await;
            assert_eq!(cands.len(), docs, "{metric} {when}");
            let plain = ids(&query(&a, &base).await);
            for lambda in [0.0, 0.3, 0.7, 1.0] {
                let (want, margin) = mmr(&cands, lambda, 6);
                assert!(
                    margin > 1e-5,
                    "{metric} {when} lambda {lambda}: margin {margin}"
                );
                let mut q = base.clone();
                q["diversify"] = json!({"lambda": lambda, "window": 40});
                let b = query(&a, &q).await;
                let got = ids(&b);
                assert_eq!(got, want, "{metric} {when} lambda {lambda}");
                if lambda == 1.0 {
                    assert_eq!(got, plain, "{metric} {when}: lambda 1 is the fused order");
                }
                if lambda == 0.3 {
                    // Diversified: the second pick is not the first pick's cluster, and the
                    // scores, which the rows keep, are no longer descending.
                    let cluster = |id: &String| id[1..].parse::<usize>().unwrap() % 3;
                    assert_ne!(
                        cluster(&got[0]),
                        cluster(&got[1]),
                        "{metric} {when} {got:?}"
                    );
                    let scores: Vec<f64> = b["results"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|r| r["score"].as_f64().unwrap())
                        .collect();
                    assert!(
                        scores.windows(2).any(|w| w[1] > w[0]),
                        "{metric} {when}: scores still descending: {scores:?}"
                    );
                }
            }
            if when == "unfolded" {
                fold(&a).await;
            }
        }
    }
}

#[tokio::test]
async fn mmr_ties_keep_the_earlier_candidate() {
    // Three identical vectors and one different. At lambda 0 every first-pick objective is 0,
    // so the first candidate wins; then the different one (similarity 0 against the first, the
    // identical ones 1); then the identical two tie, and the earlier wins: by hand,
    // d00, d03, d01, d02.
    let a = api();
    let same = [1.0, 0.0, 0.0, 0.0];
    let docs = json!([
        {"id": "d00", "vector": same}, {"id": "d01", "vector": same},
        {"id": "d02", "vector": same}, {"id": "d03", "vector": [0.0, 1.0, 0.0, 0.0]}]);
    let (s, b) = send(
        &a,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": docs,
                     "distance_metric": "euclidean_squared"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for when in ["unfolded", "folded"] {
        let plain = ids(&query(
            &a,
            &json!({"vector": [1.0, 0.1, 0.0, 0.0], "exact": true, "top_k": 4}),
        )
        .await);
        assert_eq!(
            plain,
            ["d00", "d01", "d02", "d03"],
            "{when}: the engine's order"
        );
        let b = query(
            &a,
            &json!({"vector": [1.0, 0.1, 0.0, 0.0], "exact": true, "top_k": 4,
                    "diversify": {"lambda": 0.0}}),
        )
        .await;
        assert_eq!(ids(&b), ["d00", "d03", "d01", "d02"], "{when}");
        if when == "unfolded" {
            fold(&a).await;
        }
    }
}

#[tokio::test]
async fn diversify_picks_from_the_boosted_window() {
    let a = api();
    write(&a, "cosine_distance", 0..30).await;
    fold(&a).await;
    write(&a, "cosine_distance", 30..40).await;
    let terms = json!([{"attr": "tag", "fn": "eq", "weight": 0.01, "value": 3}]);
    let base = json!({"vector": Q, "exact": true, "top_k": 6});
    // The boosted window, in the boost's order: MMR's candidates and its `s`.
    let mut boosted = base.clone();
    boosted["boost"] = json!({"window": 40, "terms": terms});
    boosted["top_k"] = json!(40);
    let b = query(&a, &boosted).await;
    let cands: Vec<Cand> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let id = r["id"].as_str().unwrap().to_owned();
            let i: usize = id[1..].parse().unwrap();
            Cand {
                id,
                score: r["score"].as_f64().unwrap(),
                v: vector(i),
            }
        })
        .collect();
    assert_eq!(cands.len(), 40);
    for lambda in [0.3, 0.7] {
        let (want, margin) = mmr(&cands, lambda, 6);
        assert!(margin > 1e-5, "lambda {lambda}: margin {margin}");
        let mut q = base.clone();
        q["boost"] = json!({"window": 40, "terms": terms});
        q["diversify"] = json!({"lambda": lambda, "window": 40});
        let b = query(&a, &q).await;
        assert_eq!(ids(&b), want, "lambda {lambda}");
        // The rows keep the boost's scores.
        for r in b["results"].as_array().unwrap() {
            let c = cands
                .iter()
                .find(|c| c.id == r["id"].as_str().unwrap())
                .unwrap();
            assert_eq!(r["score"].as_f64().unwrap(), c.score, "{r}");
        }
        // A window named on one governs both; unnamed on both, the default is shared.
        q["boost"] = json!({"terms": terms});
        q["diversify"] = json!({"lambda": lambda, "window": 40});
        assert_eq!(
            ids(&query(&a, &q).await),
            want,
            "lambda {lambda}: diversify's window"
        );
        q["boost"] = json!({"window": 40, "terms": terms});
        q["diversify"] = json!({"lambda": lambda});
        assert_eq!(
            ids(&query(&a, &q).await),
            want,
            "lambda {lambda}: boost's window"
        );
        q["boost"] = json!({"terms": terms});
        assert_eq!(query(&a, &q).await["results"].as_array().unwrap().len(), 6);
    }
    // A window on both must agree.
    let mut q = base.clone();
    q["boost"] = json!({"window": 40, "terms": terms});
    q["diversify"] = json!({"lambda": 0.5, "window": 30});
    let (s, b) = send(&a, "POST", "/v1/indexes/docs/query", Some(&q)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("diversify"),
        "{b}"
    );
}

#[tokio::test]
async fn diversify_is_refused_at_the_door() {
    let a = api();
    write(&a, "cosine_distance", 0..10).await;
    let q = |d: Value| json!({"vector": Q, "top_k": 5, "diversify": d});
    let cases = [
        ("not an object", q(json!(0.5))),
        ("an unknown field", q(json!({"lambda": 0.5, "k": 1}))),
        ("no lambda", q(json!({}))),
        ("a string lambda", q(json!({"lambda": "0.5"}))),
        ("lambda above 1", q(json!({"lambda": 1.01}))),
        ("lambda below 0", q(json!({"lambda": -0.01}))),
        ("window below top_k", q(json!({"lambda": 0.5, "window": 4}))),
        (
            "window above 1,000",
            q(json!({"lambda": 0.5, "window": 1001})),
        ),
        (
            "a fractional window",
            q(json!({"lambda": 0.5, "window": 10.5})),
        ),
        (
            "top_k above 1,000",
            json!({"vector": Q, "top_k": 1001, "diversify": {"lambda": 0.5}}),
        ),
        (
            "beside rank_by",
            json!({"rank_by": ["tag", "asc"], "top_k": 5, "diversify": {"lambda": 0.5}}),
        ),
        (
            "beside aggregate_by",
            json!({"aggregate_by": {"c": ["Count", "id"]}, "diversify": {"lambda": 0.5}}),
        ),
        (
            "beside as_of",
            json!({"vector": Q, "top_k": 5, "as_of": 1, "diversify": {"lambda": 0.5}}),
        ),
    ];
    for (what, body) in &cases {
        let (s, b) = send(&a, "POST", "/v1/indexes/docs/query", Some(body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{what}: {b}");
        assert!(
            b["error"]["message"]
                .as_str()
                .unwrap()
                .contains("diversify"),
            "{what}: the message names diversify: {b}"
        );
    }
    // The limits themselves, and `null`.
    for d in [
        json!({"lambda": 0}),
        json!({"lambda": 1}),
        json!({"lambda": 0.5, "window": 5}),
        json!({"lambda": 0.5, "window": 1000}),
        Value::Null,
    ] {
        let r = query(&a, &q(d.clone())).await;
        assert_eq!(r["results"].as_array().unwrap().len(), 5, "{d}");
    }
}

#[tokio::test]
async fn vectors_ride_the_row_round() {
    use pstore_testkit::depth::DepthCounting;
    // ⚠️ At one segment as well as four: a vector read after the rows is a chain that hides
    // behind another segment's overlapping reads (M61's finding). And with unfolded rows.
    for (segments, unfolded) in [(1, false), (1, true), (4, false), (4, true)] {
        let store = DepthCounting::new(MemoryStore::new());
        let cold = || Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
        let writer = cold();
        for k in 0..segments {
            write(&writer, "cosine_distance", k * 10..k * 10 + 10).await;
            fold(&writer).await;
        }
        let mut seen = Vec::new();
        for body in [
            json!({"vector": Q, "exact": true, "top_k": 30}),
            json!({"vector": Q, "exact": true, "top_k": 6,
                   "diversify": {"lambda": 0.5, "window": 30}}),
        ] {
            let api = cold();
            if unfolded {
                write(&api, "cosine_distance", 3..8).await;
            }
            store.reset();
            let b = query(&api, &body).await;
            assert!(!b["results"].as_array().unwrap().is_empty(), "{b}");
            let cost = &b["meta"]["cost"];
            seen.push((
                store.depth(),
                cost["blob_reads"].as_u64().unwrap(),
                cost["bytes_read"].as_u64().unwrap(),
            ));
        }
        assert_eq!(
            seen[1].0, 4,
            "{segments} segment(s), unfolded {unfolded}: {seen:?}"
        );
        // The vectors are real bytes, and real requests, beside the plain query's.
        assert!(
            seen[1].2 > seen[0].2,
            "{segments} segment(s): the vector rows read no bytes: {seen:?}"
        );
        // ...and no more than the corpus's whole vector sections (5 floats a euclidean row; 4
        // here, cosine) beyond the plain query's.
        let sections = (segments * 10 * 5 * 4) as u64;
        assert!(
            seen[1].2 <= seen[0].2 + sections,
            "{segments} segment(s): {seen:?} beyond {sections} bytes of vectors"
        );
    }
}
