//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Ranking composition through the API — M9g.1: weighted RRF and its `k`, several text
//! queries, and multi-query.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

fn api() -> Arc<Api<MemoryStore>> {
    Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).expect("a fencing backend")
}

async fn send(api: &Arc<Api<MemoryStore>>, req: Request<Body>) -> (StatusCode, Value) {
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
        .header("x-pstore-tenant", "17")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn query(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(api, request("POST", "/v1/indexes/docs/query", Some(body))).await
}

/// Six documents: `v0` nearest the query vector, `v5` farthest; the text favours the far end.
async fn corpus() -> Arc<Api<MemoryStore>> {
    let api = api();
    let texts = [
        "apple",
        "apple apple banana",
        "banana",
        "banana banana cherry",
        "cherry apple",
        "cherry cherry cherry banana apple",
    ];
    let docs: Vec<Value> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"id": format!("v{i}"), "vector": [1.0, i as f32], "text": t}))
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

fn ids(b: &Value) -> Vec<String> {
    b["results"]
        .as_array()
        .unwrap_or_else(|| panic!("{b}"))
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn scores(b: &Value) -> BTreeMap<String, f32> {
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                r["score"].as_f64().unwrap() as f32,
            )
        })
        .collect()
}

#[tokio::test]
async fn weights_and_k_shape_reciprocal_rank_fusion() {
    let api = corpus().await;
    let vector_only = query(&api, &json!({"vector": [1.0, -1.0], "top_k": 6}))
        .await
        .1;
    let silenced = query(
        &api,
        &json!({"vector": [1.0, -1.0], "text": "cherry", "top_k": 6,
                "fusion": {"rrf": {"weights": [1.0, 0.0]}}}),
    )
    .await
    .1;
    assert_eq!(
        ids(&silenced),
        ids(&vector_only),
        "a zero weight still counted"
    );
    // Whichever leg weighs 5 to 1 decides the first place -- at `k = 1`, where a rank still
    // matters; at 60 RRF is flat enough that a 5:1 weight does not decide it here.
    let weighted = |w: [f32; 2]| {
        json!({"vector": [1.0, -1.0], "text": "cherry", "top_k": 6,
               "fusion": {"rrf": {"k": 1, "weights": w}}})
    };
    let dense_first = ids(&vector_only)[0].clone();
    let text_alone = query(&api, &json!({"text": "cherry", "top_k": 1})).await.1;
    let text_first = ids(&text_alone)[0].clone();
    assert_ne!(dense_first, text_first, "the fixture proves nothing");
    let dense_heavy = query(&api, &weighted([5.0, 1.0])).await.1;
    let text_heavy = query(&api, &weighted([1.0, 5.0])).await.1;
    assert_eq!(ids(&dense_heavy)[0], dense_first, "{dense_heavy}");
    assert_eq!(ids(&text_heavy)[0], text_first, "{text_heavy}");
    // One leg, first place: exactly w / (k + 1).
    for k in [60.0f32, 10.0] {
        let b = query(
            &api,
            &json!({"vector": [1.0, -1.0], "top_k": 1,
                    "fusion": {"rrf": {"k": k, "weights": [2.0]}}}),
        )
        .await
        .1;
        assert_eq!(
            b["results"][0]["score"].as_f64().unwrap() as f32,
            2.0 / (k + 1.0)
        );
    }
}

#[tokio::test]
async fn a_multi_query_answers_each_as_it_would_alone() {
    let api = corpus().await;
    let subs = [
        json!({"vector": [1.0, 2.0], "top_k": 3}),
        json!({"text": ["apple", "cherry"], "top_k": 4,
               "fusion": {"sum": {"weights": [1.0, 2.0]}}}),
        json!({"text": ["apple", "cherry"], "top_k": 4,
               "fusion": {"rrf": {"k": 10, "weights": [1.0, 2.0]}}}),
        json!({"rank_by": ["id", "desc"], "top_k": 2}),
    ];
    let (s, multi) = query(&api, &json!({"queries": subs})).await;
    assert_eq!(s, StatusCode::OK, "{multi}");
    assert_eq!(multi["meta"]["cost"]["blob_lists"], 0);
    for (i, sub) in subs.iter().enumerate() {
        let alone = query(&api, sub).await.1;
        assert_eq!(multi["results"][i], alone["results"], "query {i}");
    }
}

#[tokio::test]
async fn composition_refuses_what_it_cannot_mean() {
    let api = corpus().await;
    let too_many_texts: Vec<&str> = std::iter::repeat_n("apple", 16).collect();
    let many_queries: Vec<Value> = (0..17).map(|_| json!({"vector": [1.0, 0.0]})).collect();
    for body in [
        json!({"vector": [1.0, 0.0], "fusion": {"borda": {}}}),
        json!({"vector": [1.0, 0.0], "fusion": {"rrf": {"weights": [1.0, 1.0]}}}),
        json!({"vector": [1.0, 0.0], "fusion": {"rrf": {"weights": [-1.0]}}}),
        json!({"vector": [1.0, 0.0], "fusion": {"rrf": {"k": 0}}}),
        json!({"vector": [1.0, 0.0], "fusion": "rrf"}),
        json!({"vector": [1.0, 0.0], "text": too_many_texts}),
        json!({"vector": [1.0, 0.0], "text": []}),
        json!({"queries": []}),
        json!({"queries": many_queries}),
        json!({"queries": [{"vector": [1.0, 0.0]}], "top_k": 3}),
        json!({"queries": [{"queries": [{"vector": [1.0, 0.0]}], "vector": [1.0, 0.0]}]}),
        json!({"queries": [{"vector": [1.0, 0.0]}, {"text": "x", "fusion": {"rrf": {"weights": []}}}]}),
        json!({"vector": [1.0, 0.0], "fusion": {"rrf": {"kk": 5}}}),
        json!({"rank_by": ["id", "asc"], "fusion": {"borda": {}}}),
        json!({"rank_by": ["id", "asc"], "fusion": {"rrf": {"k": 5}}}),
        // A dense leg under `sum` or `max` (M9g.2): its scores can be negative.
        json!({"vector": [1.0, 0.0], "text": "apple", "fusion": {"sum": {}}}),
        json!({"vector": [1.0, 0.0], "text": "apple", "fusion": {"max": {}}}),
        json!({"text": ["apple", "cherry"], "fusion": {"max": {"weights": [0.0, 0.0]}}}),
        // `k` is RRF's alone: under `sum` or `max` it would be read as nothing (code review).
        json!({"text": ["apple", "cherry"], "fusion": {"sum": {"k": 5}}}),
        json!({"text": ["apple", "cherry"], "fusion": {"max": {"k": 5}}}),
    ] {
        let (s, b) = query(&api, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
    }
    // Sixteen legs is the bound, and allowed.
    let fifteen: Vec<&str> = std::iter::repeat_n("apple", 15).collect();
    let (s, b) = query(&api, &json!({"vector": [1.0, 0.0], "text": fifteen})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn several_text_queries_are_legs_of_their_own() {
    let api = corpus().await;
    // One string and a one-string array are the same leg.
    let one = query(&api, &json!({"text": "cherry", "top_k": 6})).await.1;
    let array = query(&api, &json!({"text": ["cherry"], "top_k": 6}))
        .await
        .1;
    assert_eq!(one["results"], array["results"]);
    // Two queries rank what either matches: every row with `apple` or `cherry`, and only those
    // (v2 is `banana` alone).
    let both = query(&api, &json!({"text": ["apple", "cherry"], "top_k": 6}))
        .await
        .1;
    let mut got = ids(&both);
    got.sort();
    assert_eq!(got, ["v0", "v1", "v3", "v4", "v5"]);
    // A weight on the second leg alone moves its best hit to the top.
    let heavy = query(
        &api,
        &json!({"text": ["apple", "cherry"], "top_k": 6,
                "fusion": {"rrf": {"weights": [0.0, 1.0]}}}),
    )
    .await
    .1;
    let cherry_first = ids(&query(&api, &json!({"text": "cherry", "top_k": 1})).await.1);
    assert_eq!(ids(&heavy)[0], cherry_first[0]);
}

#[tokio::test]
async fn sum_and_max_combine_each_text_legs_own_score() {
    let api = corpus().await;
    // Each leg's raw BM25, from the leg alone under `sum` with its default weight of 1.
    let alone = |q: &str| json!({"text": q, "top_k": 6, "fusion": {"sum": {}}});
    let a = scores(&query(&api, &alone("apple")).await.1);
    let c = scores(&query(&api, &alone("cherry")).await.1);
    let both = |kind: &str, w: [f32; 2]| {
        json!({"text": ["apple", "cherry"], "top_k": 6,
               "fusion": {kind: {"weights": w}}})
    };
    for (kind, w) in [
        ("sum", [1.0, 1.0]),
        ("sum", [2.0, 0.5]),
        ("max", [1.0, 1.0]),
        ("max", [0.5, 3.0]),
    ] {
        let got = query(&api, &both(kind, w)).await.1;
        let mut want: Vec<(String, f32)> = a
            .keys()
            .chain(c.keys())
            .map(|id| {
                let (x, y) = (a.get(id).map(|s| w[0] * s), c.get(id).map(|s| w[1] * s));
                let v = match kind {
                    "sum" => x.unwrap_or(0.0) + y.unwrap_or(0.0),
                    _ => x.into_iter().chain(y).fold(f32::NEG_INFINITY, f32::max),
                };
                (id.clone(), v)
            })
            .collect();
        want.sort_by(|p, q| q.1.total_cmp(&p.1).then_with(|| p.0.cmp(&q.0)));
        want.dedup_by(|p, q| p.0 == q.0);
        let got_scores = scores(&got);
        for (id, v) in &want {
            let g = got_scores[id];
            assert!(
                (g - v).abs() <= 1e-5 * v.abs().max(1.0),
                "{kind} {w:?}: {id} {g} vs {v}"
            );
        }
        assert_eq!(got_scores.len(), want.len(), "{kind}: {got}");
    }
}

#[tokio::test]
async fn a_sum_finds_the_best_total_that_no_leg_ranks_first() {
    // Spec review: X has both terms, Y only more `alpha`, Z only more `beta`. Each leg alone
    // ranks Y or Z first; X has the largest sum. A leg cut at `top_k` would never see X.
    let api = api();
    let docs = json!([
        {"id": "x", "vector": [1.0, 0.0], "text": "alpha beta"},
        {"id": "y", "vector": [1.0, 0.0], "text": "alpha alpha alpha"},
        {"id": "z", "vector": [1.0, 0.0], "text": "beta beta beta"},
        {"id": "w", "vector": [1.0, 0.0], "text": "gamma"},
    ]);
    let body = json!({"durability": "durable", "documents": docs});
    let (s, b) = send(
        &api,
        request("PUT", "/v1/indexes/docs/documents", Some(&body)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    send(&api, request("POST", "/v1/admin/fold", None)).await;
    let first = |q: &str| json!({"text": q, "top_k": 1, "fusion": {"sum": {}}});
    assert_eq!(ids(&query(&api, &first("alpha")).await.1), ["y"]);
    assert_eq!(ids(&query(&api, &first("beta")).await.1), ["z"]);
    let both = json!({"text": ["alpha", "beta"], "top_k": 1, "fusion": {"sum": {}}});
    assert_eq!(ids(&query(&api, &both).await.1), ["x"]);
}

#[tokio::test]
async fn a_sums_shadow_check_does_not_grow_with_the_rows_it_matches() {
    // Spec review of M9g.2: `sum`'s legs are whole, so checking the shadow on every candidate
    // would read a block per matching row. Fused first, only `top_k + |shadow|` are checked.
    let mut added = Vec::new();
    for n in [500usize, 2000] {
        let api = api();
        let docs: Vec<Value> = (0..n)
            .map(|i| {
                json!({"id": format!("d{i:05}"), "vector": [1.0, 0.0],
                            "text": format!("alpha beta w{i}")})
            })
            .collect();
        let body = json!({"durability": "durable", "documents": docs});
        send(
            &api,
            request("PUT", "/v1/indexes/docs/documents", Some(&body)),
        )
        .await;
        send(&api, request("POST", "/v1/admin/fold", None)).await;
        let q = json!({"text": ["alpha", "beta"], "top_k": 5, "fusion": {"sum": {}}});
        let bytes = |b: &Value| b["meta"]["cost"]["bytes_read"].as_u64().unwrap();
        let clean = bytes(&query(&api, &q).await.1);
        let one = json!({"documents": [{"id": "elsewhere", "vector": [1.0, 0.0], "text": "zeta"}]});
        send(
            &api,
            request("PUT", "/v1/indexes/docs/documents", Some(&one)),
        )
        .await;
        let shadowed = bytes(&query(&api, &q).await.1);
        added.push(shadowed.saturating_sub(clean));
    }
    // Measured: 0 and 0 fused first; 18,304 and 82,400 bytes when every candidate is checked.
    assert!(
        added[1] <= added[0] + 16 * 1024,
        "the shadow check grew with the matching rows: {added:?}"
    );
}

#[tokio::test]
async fn a_sum_never_serves_a_row_an_unfolded_write_superseded() {
    // The fused-then-resolved shadow must still drop a folded row whose id was rewritten.
    let api = api();
    let docs = json!([
        {"id": "x", "vector": [1.0, 0.0], "text": "alpha beta"},
        {"id": "y", "vector": [1.0, 0.0], "text": "alpha"},
        {"id": "w", "vector": [1.0, 0.0], "text": "gamma"},
    ]);
    let body = json!({"durability": "durable", "documents": docs});
    send(
        &api,
        request("PUT", "/v1/indexes/docs/documents", Some(&body)),
    )
    .await;
    send(&api, request("POST", "/v1/admin/fold", None)).await;
    let q = json!({"text": ["alpha", "beta"], "top_k": 1, "fusion": {"sum": {}}});
    assert_eq!(ids(&query(&api, &q).await.1), ["x"]);
    // x rewritten without either term, unfolded: its folded version must not answer.
    let again = json!({"documents": [{"id": "x", "vector": [1.0, 0.0], "text": "delta"}]});
    send(
        &api,
        request("PUT", "/v1/indexes/docs/documents", Some(&again)),
    )
    .await;
    assert_eq!(ids(&query(&api, &q).await.1), ["y"]);
}
