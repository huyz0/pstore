//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Outer RRF over a multi-query through the API — M142.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_server::Api;
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceExt;

async fn send<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    req: Request<Body>,
) -> (StatusCode, Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn request(method: &str, uri: &str, body: Option<&Value>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "18")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn query<S: BlobStore + 'static>(api: &Arc<Api<S>>, body: &Value) -> (StatusCode, Value) {
    send(api, request("POST", "/v1/indexes/docs/query", Some(body))).await
}

/// Eight documents: `v0` nearest the query vector; the text favours the far end.
async fn corpus<S: BlobStore + 'static>(store: Accounted<S>) -> Arc<Api<S>> {
    let api = Api::unsecured(store, LaneId(1)).unwrap();
    let texts = [
        "apple",
        "apple apple banana",
        "banana",
        "banana banana cherry",
        "cherry apple",
        "cherry cherry cherry banana apple",
        "cherry",
        "banana",
    ];
    let docs: Vec<Value> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| {
            json!({"id": format!("v{i}"), "vector": [1.0, i as f32], "text": t,
                   "attributes": {"n": i, "tag": format!("t{i}")}})
        })
        .collect();
    let body = json!({"durability": "durable", "documents": docs});
    let (s, b) = send(
        &api,
        request("PUT", "/v1/indexes/docs/documents", Some(&body)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(&api, request("POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    api
}

/// A dense query, a text query and an ordered query with its own offset; two ask for
/// different attributes.
fn three() -> Vec<Value> {
    vec![
        json!({"vector": [1.0, 2.0], "top_k": 5, "include_attributes": ["n"]}),
        json!({"text": "cherry", "top_k": 4, "include_attributes": ["tag"]}),
        json!({"rank_by": ["id", "desc"], "top_k": 4, "offset": 1}),
    ]
}

/// The spec's fusion, over each query's answer run alone: ids, `f32` scores, attributes.
fn reference(
    answers: &[Value],
    k: f64,
    weights: &[f64],
    offset: usize,
    limit: usize,
) -> Vec<(String, f32, Value)> {
    let mut score: HashMap<String, f64> = HashMap::new();
    let mut attrs: HashMap<String, Value> = HashMap::new();
    for (q, a) in answers.iter().enumerate() {
        for (i, r) in a["results"].as_array().unwrap().iter().enumerate() {
            let id = r["id"].as_str().unwrap().to_owned();
            *score.entry(id.clone()).or_insert(0.0) += weights[q] / (k + (i + 1) as f64);
            if !r["attributes"].is_null() {
                attrs.entry(id).or_insert_with(|| r["attributes"].clone());
            }
        }
    }
    let mut all: Vec<(String, f64)> = score.into_iter().collect();
    all.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| a.0.as_bytes().cmp(b.0.as_bytes()))
    });
    all.into_iter()
        .skip(offset)
        .take(limit)
        .map(|(id, s)| {
            let a = attrs.get(&id).cloned().unwrap_or(Value::Null);
            (id, s as f32, a)
        })
        .collect()
}

fn fused(b: &Value) -> Vec<(String, f32, Value)> {
    let results = b["results"].as_array().unwrap_or_else(|| panic!("{b}"));
    assert_eq!(results.len(), 1, "one fused list: {b}");
    results[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            assert!(r.get("$dist").is_none(), "{r}");
            (
                r["id"].as_str().unwrap().to_owned(),
                r["score"].as_f64().unwrap() as f32,
                r.get("attributes").cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
}

#[tokio::test]
async fn the_fused_answer_is_the_reference_over_each_querys_own_answer() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    let mut answers = Vec::new();
    for q in three() {
        let (s, b) = query(&api, &q).await;
        assert_eq!(s, StatusCode::OK, "{q} -> {b}");
        answers.push(b);
    }
    let ids = |a: &Value| -> Vec<String> {
        a["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_owned())
            .collect()
    };
    // The fixture: overlapping answers, so deduplication is exercised.
    let (a, b) = (ids(&answers[0]), ids(&answers[1]));
    assert!(a.iter().any(|i| b.contains(i)), "no overlap: {a:?} {b:?}");
    let cases = [
        (
            json!(["RRF"]),
            None,
            None,
            60.0,
            vec![1.0, 1.0, 1.0],
            0,
            usize::MAX,
        ),
        (
            json!(["RRF", {"rank_constant": 2, "weights": [3.0, 0.5, 1.25]}]),
            None,
            None,
            2.0,
            vec![3.0, 0.5, 1.25],
            0,
            usize::MAX,
        ),
        (
            json!(["RRF", {"rank_constant": 5}]),
            Some(json!(3)),
            Some(json!(2)),
            5.0,
            vec![1.0, 1.0, 1.0],
            2,
            3,
        ),
        (
            json!(["RRF"]),
            Some(json!({"total": 4})),
            None,
            60.0,
            vec![1.0, 1.0, 1.0],
            0,
            4,
        ),
    ];
    for (rerank, limit, offset, k, weights, off, lim) in cases {
        let mut body = json!({"queries": three(), "rerank_by": rerank});
        if let Some(l) = limit {
            body["limit"] = l;
        }
        if let Some(o) = offset {
            body["offset"] = o;
        }
        let (s, got) = query(&api, &body).await;
        assert_eq!(s, StatusCode::OK, "{body} -> {got}");
        let want = reference(&answers, k, &weights, off, lim);
        assert_eq!(fused(&got), want, "{body}");
        let mut seen = std::collections::HashSet::new();
        assert!(
            fused(&got).iter().all(|(id, _, _)| seen.insert(id.clone())),
            "an id twice"
        );
        // Meta is still one entry per query.
        assert_eq!(got["meta"]["epochs"].as_array().unwrap().len(), 3, "{got}");
    }
    // `{"total": n}` answers as `n` does.
    let total = query(
        &api,
        &json!({"queries": three(), "rerank_by": ["RRF"], "limit": {"total": 4}}),
    )
    .await
    .1;
    let plain = query(
        &api,
        &json!({"queries": three(), "rerank_by": ["RRF"], "limit": 4}),
    )
    .await
    .1;
    assert_eq!(total["results"], plain["results"]);
}

#[tokio::test]
async fn equal_fused_scores_order_by_id_whatever_the_query_order() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    // v0 and v7 each first in one query alone: equal by construction.
    let first = json!({"rank_by": ["id", "asc"], "top_k": 1});
    let last = json!({"rank_by": ["id", "desc"], "top_k": 1});
    for queries in [vec![first.clone(), last.clone()], vec![last, first]] {
        let (s, b) = query(&api, &json!({"queries": queries, "rerank_by": ["RRF"]})).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let ids: Vec<String> = fused(&b).into_iter().map(|(id, _, _)| id).collect();
        assert_eq!(ids, ["v0", "v7"]);
    }
}

#[tokio::test]
async fn what_cannot_mean_an_outer_rrf_is_refused_before_any_blob_request() {
    let store = Accounted::new(MemoryStore::new());
    let api = corpus(store.clone()).await;
    let spent =
        || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(18), c));
    let two = || vec![json!({"vector": [1.0, 0.0]}), json!({"text": "apple"})];
    for body in [
        json!({"queries": two(), "rerank_by": "RRF"}),
        json!({"queries": two(), "rerank_by": ["RRF", {"rank_constant": 0}]}),
        json!({"queries": two(), "rerank_by": ["RRF", {"rank_constant": 60.0}]}),
        json!({"queries": two(), "rerank_by": ["RRF", {"weights": [1.0]}]}),
        json!({"queries": two(), "rerank_by": ["RRF", {"weights": [1.0, 0.0]}]}),
        json!({"queries": two(), "rerank_by": ["RRF", {"weights": [1.0, 2_000_000.0]}]}),
        json!({"queries": two(), "rerank_by": ["RRF", {"k": 60}]}),
        json!({"queries": two(), "rerank_by": {"model": "m", "attribute": "a", "query": "q"}}),
        json!({"queries": two(), "limit": 3}),
        json!({"queries": two(), "offset": 1}),
        json!({"queries": two(), "rerank_by": ["RRF"], "limit": -1}),
        json!({"queries": two(), "rerank_by": ["RRF"], "limit": {}}),
        json!({"queries": two(), "rerank_by": ["RRF"], "limit": {"total": 2, "x": 1}}),
        json!({"queries": two(), "rerank_by": ["RRF"], "offset": 1.5}),
        json!({"queries": two(), "rerank_by": ["RRF"], "top_k": 3}),
    ] {
        let before = spent();
        let (s, b) = query(&api, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
        assert_eq!(b["error"]["code"], "bad_request", "{body} -> {b}");
        assert_eq!(before, spent(), "{body}: the refusal made blob requests");
    }
}

#[tokio::test]
async fn fusion_adds_no_depth_and_no_request() {
    let queries = three();
    let mut seen = Vec::new();
    for body in [
        json!({"queries": queries.clone()}),
        json!({"queries": queries.clone(), "rerank_by": ["RRF"], "limit": 3}),
    ] {
        // A cold server each, so neither run reads what the other warmed. Clone shares the
        // counters.
        let store = DepthCounting::new(MemoryStore::new());
        let api = corpus(Accounted::new(store.clone())).await;
        store.reset();
        let (s, b) = query(&api, &body).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(
            store.depth() > 0 && store.requests() > 0,
            "nothing was measured"
        );
        seen.push((store.depth(), store.requests()));
    }
    assert_eq!(seen[0], seen[1], "plain, fused");
}
