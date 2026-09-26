//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Distance metrics, `$dist`, `exact` and base64 vectors through the API — M9d.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
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
        .header("x-pstore-tenant", "14")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn put(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(
        api,
        request("PUT", "/v1/indexes/docs/documents", Some(body)),
    )
    .await
}

async fn query(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(api, request("POST", "/v1/indexes/docs/query", Some(body))).await
}

async fn fold(api: &Arc<Api<MemoryStore>>) {
    let (s, b) = send(api, request("POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Deterministic vectors whose norms spread over more than 10x, so a skipped normalization
/// or a flipped augmentation changes the answer.
fn corpus(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut state = seed;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    (0..n)
        .map(|i| {
            let scale = 1.0 + 19.0 * (i as f32 / n as f32);
            (0..dim).map(|_| next() * scale).collect()
        })
        .collect()
}

fn brute(metric: &str, q: &[f32], v: &[f32]) -> f32 {
    let dot: f32 = q.iter().zip(v).map(|(a, b)| a * b).sum();
    let norm = |x: &[f32]| x.iter().map(|a| a * a).sum::<f32>().sqrt();
    match metric {
        "cosine_distance" => 1.0 - dot / (norm(q) * norm(v)),
        "euclidean_squared" => q.iter().zip(v).map(|(a, b)| (a - b) * (a - b)).sum(),
        _ => -dot,
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-3 + 1e-3 * b.abs()
}

async fn check_exact(api: &Arc<Api<MemoryStore>>, metric: &str, docs: &[Vec<f32>], q: &[f32]) {
    let (s, b) = query(api, &json!({"vector": q, "top_k": 10, "exact": true})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut truth: Vec<(f32, usize)> = docs
        .iter()
        .enumerate()
        .map(|(i, v)| (brute(metric, q, v), i))
        .collect();
    truth.sort_by(|a, b| a.0.total_cmp(&b.0));
    let got = b["results"].as_array().unwrap();
    assert_eq!(got.len(), 10, "{b}");
    let tenth = truth[9].0;
    for (rank, r) in got.iter().enumerate() {
        let id: usize = r["id"].as_str().unwrap()[1..].parse().unwrap();
        let dist = r["$dist"].as_f64().expect("a dense hit carries $dist") as f32;
        let want = brute(metric, q, &docs[id]);
        assert!(
            close(dist, want),
            "{metric}: {id} has $dist {dist}, brute force {want}"
        );
        // In the true top 10, allowing ties at the boundary.
        assert!(
            want <= tenth || close(want, tenth),
            "{metric}: {id} ({want}) is not in the top 10 (10th is {tenth})"
        );
        // And in order.
        assert!(
            close(dist, truth[rank].0) || dist >= truth[rank].0,
            "{metric}: rank {rank} out of order"
        );
    }
}

#[tokio::test]
async fn each_metric_ranks_and_measures_as_brute_force_does() {
    for metric in ["cosine_distance", "euclidean_squared", "dot_product"] {
        let api = api();
        let docs = corpus(60, 6, 0x9E37_79B9_7F4A_7C15);
        let body = json!({
            "durability": "durable",
            "distance_metric": metric,
            "documents": docs.iter().enumerate()
                .map(|(i, v)| json!({"id": format!("d{i}"), "vector": v}))
                .collect::<Vec<_>>(),
        });
        let (s, b) = put(&api, &body).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        for q in corpus(3, 6, 0xDEAD_BEEF) {
            check_exact(&api, metric, &docs, &q).await;
        }
        fold(&api).await;
        for q in corpus(3, 6, 0xDEAD_BEEF) {
            check_exact(&api, metric, &docs, &q).await;
        }
        let (_, stats) = send(&api, request("GET", "/v1/indexes/docs", None)).await;
        assert_eq!(stats["schema"]["distance_metric"], metric, "{stats}");
        assert_eq!(stats["schema"]["dims"], 6, "the client's width: {stats}");
    }
}

#[tokio::test]
async fn a_base64_vector_is_the_same_vector() {
    let b64 = |v: &[f32]| {
        let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    };
    let docs = corpus(20, 4, 7);
    let api = api();
    let body = json!({"distance_metric": "cosine_distance", "documents": docs.iter().enumerate()
        .map(|(i, v)| json!({"id": format!("d{i}"), "vector": b64(v)})).collect::<Vec<_>>()});
    assert_eq!(put(&api, &body).await.0, StatusCode::OK);
    let q = corpus(1, 4, 9).remove(0);
    let (_, by_array) = query(&api, &json!({"vector": q, "top_k": 5, "exact": true})).await;
    let (_, by_b64) = query(&api, &json!({"vector": b64(&q), "top_k": 5, "exact": true})).await;
    assert_eq!(by_array["results"], by_b64["results"]);
    check_exact(&api, "cosine_distance", &docs, &q).await;
}

#[tokio::test]
async fn malformed_vectors_metrics_and_reserved_names_are_refused() {
    let api = api();
    let one = |vector: Value, extra: Value| {
        let mut body = json!({"documents": [{"id": "a", "vector": vector}]});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        body
    };
    let cases = [
        one(json!("not base64!"), json!({})),
        one(json!("AAAAAAA="), json!({})), // 5 bytes: not a whole f32
        one(json!("AADAfwAAgD8="), json!({})), // NaN, 1.0
        one(json!([1.0, 2.0]), json!({"distance_metric": "manhattan"})),
        one(
            json!([0.0, 0.0]),
            json!({"distance_metric": "cosine_distance"}),
        ),
        json!({"documents": [{"id": "a", "vector": [1.0], "attributes": {"$x": 1}}]}),
    ];
    for body in cases {
        let (s, b) = put(&api, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
    }
    // A zero query has no direction under cosine either.
    let ok = one(
        json!([1.0, 2.0]),
        json!({"distance_metric": "cosine_distance"}),
    );
    assert_eq!(put(&api, &ok).await.0, StatusCode::OK);
    let (s, b) = query(&api, &json!({"vector": [0.0, 0.0]})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, b) = query(&api, &json!({"vector": "@@"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}

#[tokio::test]
async fn a_second_metric_is_refused_at_every_rung_the_width_is() {
    let api = api();
    let doc = |id: &str, metric: &str| json!({"distance_metric": metric, "documents": [{"id": id, "vector": [1.0, 2.0]}]});
    assert_eq!(
        put(&api, &doc("a", "cosine_distance")).await.0,
        StatusCode::OK
    );
    // Unfolded rows: same width, another metric.
    let (s, b) = put(&api, &doc("b", "dot_product")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    fold(&api).await;
    // The schema the fold recorded.
    let (s, b) = put(&api, &doc("c", "dot_product")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (_, r) = query(&api, &json!({"vector": [1.0, 2.0], "top_k": 10})).await;
    let ids: Vec<&str> = r["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a"]);
}

#[tokio::test]
async fn a_text_only_hit_carries_no_dist_and_a_dense_one_does() {
    let api = api();
    let body = json!({"distance_metric": "euclidean_squared", "documents": [
        {"id": "near", "vector": [1.0, 0.0], "text": "zebra"},
        {"id": "far", "vector": [9.0, 9.0], "text": "zebra"},
    ]});
    assert_eq!(put(&api, &body).await.0, StatusCode::OK);
    let (_, r) = query(&api, &json!({"text": "zebra", "top_k": 10})).await;
    for row in r["results"].as_array().unwrap() {
        assert!(row.get("$dist").is_none(), "{row}");
    }
    let (_, r) = query(&api, &json!({"vector": [1.0, 0.0], "top_k": 1})).await;
    assert_eq!(r["results"][0]["id"], "near", "{r}");
    assert!(
        close(r["results"][0]["$dist"].as_f64().unwrap() as f32, 0.0),
        "{r}"
    );
}

#[tokio::test]
async fn the_metric_a_row_carries_is_never_returned_or_filtered_on() {
    // Code review of M9d: `$metric` rides each unfolded row to the fold, and must be stripped
    // from the fresh view too, not only before a fold seals.
    let api = api();
    let body = json!({"distance_metric": "cosine_distance",
        "documents": [{"id": "a", "vector": [1.0, 2.0], "attributes": {"n": 1}}]});
    assert_eq!(put(&api, &body).await.0, StatusCode::OK);
    for folded in [false, true] {
        if folded {
            fold(&api).await;
        }
        let (_, r) = query(
            &api,
            &json!({"vector": [1.0, 2.0], "include_attributes": true}),
        )
        .await;
        assert_eq!(
            r["results"][0]["attributes"],
            json!({"n": 1}),
            "folded {folded}: {r}"
        );
        let (_, r) = query(
            &api,
            &json!({"vector": [1.0, 2.0], "filters": ["$metric", "Eq", 1]}),
        )
        .await;
        assert_eq!(r["results"], json!([]), "folded {folded}: {r}");
    }
}

#[tokio::test]
async fn a_vector_too_large_to_measure_is_refused() {
    // Code review of M9d: components near 2e19 overflow the f32 norm to infinity, which
    // stored a cosine vector as zeros and a euclidean one with a -inf component.
    let api = api();
    for metric in ["cosine_distance", "euclidean_squared"] {
        let body = json!({"distance_metric": metric,
            "documents": [{"id": "a", "vector": [3.0e19, 1.0]}]});
        let (s, b) = put(&api, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{metric}: {b}");
        // And as a query, against an index of that metric.
        let api = self::api();
        let ok = json!({"distance_metric": metric,
            "documents": [{"id": "a", "vector": [3.0, 1.0]}]});
        assert_eq!(put(&api, &ok).await.0, StatusCode::OK);
        let (s, b) = query(&api, &json!({"vector": [3.0e19, 1.0]})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{metric} query: {b}");
    }
}
