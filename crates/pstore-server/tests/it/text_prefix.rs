//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M180: `last_as_prefix` on a text leg object, over HTTP.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "180")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn query(api: &A, body: &Value) -> (StatusCode, Value) {
    send(api, "POST", "/v1/indexes/p/query", body).await
}

fn ids(b: &Value) -> Vec<(String, f64)> {
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                r["score"].as_f64().unwrap(),
            )
        })
        .collect()
}

fn prefixed(q: &str, last_as_prefix: Value) -> Value {
    json!({"text": [{"field": "text", "query": q, "last_as_prefix": last_as_prefix}],
           "fusion": {"sum": {"weights": [1.0]}}, "top_k": 10})
}

/// Criterion 4: the option ranks a `walrus` row for `wal`, unfolded and folded; a non-bool is
/// refused naming it; `false` is the leg without it.
#[tokio::test]
async fn text_legs_take_last_as_prefix() {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    let docs: Vec<Value> = ["walrus tusks", "lazy fox", "a walnut tree", "lazy walrus"]
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"id": format!("d{i}"), "vector": [1.0, i as f32], "text": t}))
        .collect();
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/p/documents",
        &json!({"durability": "durable", "documents": docs}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for folded in [false, true] {
        if folded {
            let (s, b) = send(&api, "POST", "/v1/admin/fold", &json!({})).await;
            assert_eq!(s, StatusCode::OK, "{b}");
        }
        let (s, b) = query(&api, &prefixed("wal", json!(true))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let mut got = ids(&b);
        got.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(
            got,
            [
                ("d0".to_owned(), 1.0),
                ("d2".to_owned(), 1.0),
                ("d3".to_owned(), 1.0)
            ],
            "folded {folded}"
        );
        let (s, b) = query(&api, &prefixed("lazy wal", json!(true))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(ids(&b)[0].0, "d3", "folded {folded}: {b}");
        // `false` is the leg without the option, byte for byte.
        let (s0, without) = query(
            &api,
            &json!({"text": [{"field": "text", "query": "lazy wal"}],
                    "fusion": {"sum": {"weights": [1.0]}}, "top_k": 10}),
        )
        .await;
        let (s1, off) = query(&api, &prefixed("lazy wal", json!(false))).await;
        assert_eq!((s0, s1), (StatusCode::OK, StatusCode::OK));
        assert_eq!(off["results"], without["results"], "folded {folded}");
        assert!(!ids(&off).iter().any(|(id, _)| id == "d0"));
    }
    for bad in [json!("yes"), json!(1), json!(null)] {
        let (s, b) = query(&api, &prefixed("wal", bad.clone())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {b}");
        assert!(b.to_string().contains("last_as_prefix"), "{bad}: {b}");
    }
}
