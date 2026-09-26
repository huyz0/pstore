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
        // `sum` and `max` are M9g.2's; until then, unknown kinds.
        json!({"text": "apple", "fusion": {"sum": {}}}),
        json!({"text": "apple", "fusion": {"max": {}}}),
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
