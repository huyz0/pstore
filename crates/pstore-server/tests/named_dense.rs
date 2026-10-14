//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M77: a dense query over a named field, through the API.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

const TENANT: u128 = 77;

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

fn tag(i: usize) -> Vec<f32> {
    let x = (i * 13) as f32;
    vec![x.sin(), (x * 0.7).cos(), (x * 0.3).sin()]
}

fn doc(i: usize) -> Value {
    let x = i as f32;
    json!({"id": format!("d{i:03}"), "vector": [x.cos(), x.sin(), 1.0, 0.5],
           "vectors": {"tag": [tag(i)], "late": [tag(i), tag(i + 1)]}})
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

async fn setup(metric: &str) -> Arc<Api<MemoryStore>> {
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    let docs: Vec<Value> = (0..60).map(doc).collect();
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": docs, "distance_metric": metric})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(&api, "POST", "/v1/admin/fold", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    api
}

async fn ids(api: &Arc<Api<MemoryStore>>, q: &Value) -> Vec<String> {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", Some(q)).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_api_searches_a_named_field() {
    let api = setup("dot_product").await;
    let q = [0.3f32, -0.8, 0.5];
    let got = ids(&api, &json!({"vector": q, "field": "tag", "top_k": 10})).await;
    let mut want: Vec<(usize, f32)> = (0..60).map(|i| (i, dot(&q, &tag(i)))).collect();
    want.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let want: Vec<String> = want
        .iter()
        .take(10)
        .map(|(i, _)| format!("d{i:03}"))
        .collect();
    assert_eq!(got, want);
    // `vector` is unchanged: a 4-wide query ranks by the default field.
    let by_vector = ids(&api, &json!({"vector": [0.3, -0.8, 0.5, 0.1], "top_k": 10})).await;
    assert_ne!(by_vector, got);
    let named_vector = ids(
        &api,
        &json!({"vector": [0.3, -0.8, 0.5, 0.1], "field": "vector", "top_k": 10}),
    )
    .await;
    assert_eq!(named_vector, by_vector);

    // Refusals: a reserved name, a field no document carries (as the engine says), a width
    // that is not the field's, and a field of several vectors.
    for (body, why) in [
        (json!({"vector": q, "field": "tag$fde"}), "$fde"),
        (json!({"vector": q, "field": "nope"}), "unknown_field"),
        (json!({"vector": [1.0, 2.0], "field": "tag"}), "dimension"),
        (json!({"vector": q, "field": "late"}), "multi"),
    ] {
        let (s, b) = send(&api, "POST", "/v1/indexes/docs/query", Some(&body)).await;
        assert!(s.is_client_error(), "{body} -> {s} {b}");
        assert!(b.to_string().contains(why), "{body} -> {b}");
    }
}

#[tokio::test]
async fn a_named_field_follows_the_index_metric() {
    let api = setup("cosine_distance").await;
    let q = [0.3f32, -0.8, 0.5];
    let (s, b) = send(
        &api,
        "POST",
        "/v1/indexes/docs/query",
        Some(&json!({"vector": q, "field": "tag", "top_k": 5})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let cos = |a: &[f32], b: &[f32]| dot(a, b) / (dot(a, a).sqrt() * dot(b, b).sqrt());
    let mut want: Vec<(usize, f32)> = (0..60).map(|i| (i, cos(&q, &tag(i)))).collect();
    want.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    for (r, (i, c)) in b["results"].as_array().unwrap().iter().zip(want) {
        assert_eq!(r["id"], format!("d{i:03}"));
        let d = r["$dist"]
            .as_f64()
            .unwrap_or_else(|| panic!("no $dist: {r}"));
        assert!((d - f64::from(1.0 - c)).abs() < 1e-4, "{d} vs {}", 1.0 - c);
    }
}
