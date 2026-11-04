//! M162: `POST /v1/indexes/{index}/recall` measures an index's recall at one snapshot.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::{Api, FoldPolicy};
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const TENANT: u64 = 162;

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT.to_string())
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn recall(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    send(api, "POST", &format!("/v1/indexes/{index}/recall"), body).await
}

#[tokio::test]
async fn recall_route() {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    let docs: Vec<Value> = (0..40)
        .map(|i| {
            json!({"id": format!("d{i:02}"), "vector": [f64::from(i).sin(), f64::from(i).cos()],
                   "attributes": {"even": i % 2 == 0}})
        })
        .collect();
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/t/documents",
        &json!({"documents": docs, "durability": "durable"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    api.fold_due(&FoldPolicy {
        period: std::time::Duration::from_secs(1),
        age: std::time::Duration::ZERO,
        bytes: 1,
    })
    .await;
    let (_, summary) = send(&api, "GET", "/v1/indexes/t", &Value::Null).await;

    // Defaults: 10 queries of 10, over an index scanned exactly.
    let (s, b) = recall(&api, "t", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (
            &b["avg_recall"],
            &b["avg_ann_count"],
            &b["avg_exhaustive_count"],
            &b["num"]
        ),
        (&json!(1.0), &json!(10.0), &json!(10.0), &json!(10)),
        "{b}"
    );
    assert_eq!(b["epoch"], summary["epoch"], "{b}");
    assert!(b.get("ground_truth").is_none(), "{b}");
    assert_eq!(b["cost"]["blob_lists"], 0, "{b}");
    assert_eq!(b["cost"]["blob_writes"], 0, "{b}");
    assert!(b["cost"]["blob_reads"].as_u64().unwrap() >= 1, "{b}");

    // With a filter and the ground truth: every neighbour satisfies it.
    let (s, b) = recall(
        &api,
        "t",
        &json!({"num": 3, "top_k": 4, "seed": 9, "include_ground_truth": true,
                "filters": ["even", "Eq", true]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let truth = b["ground_truth"].as_array().unwrap();
    assert_eq!((b["num"].clone(), truth.len()), (json!(3), 3), "{b}");
    for g in truth {
        assert_eq!(g["query_vector"].as_array().unwrap().len(), 2, "{g}");
        let near = g["nearest_neighbors"].as_array().unwrap();
        assert_eq!(near.len(), 4, "{g}");
        for n in near {
            let id: u32 = n["id"].as_str().unwrap()[1..].parse().unwrap();
            assert_eq!(id % 2, 0, "{n}");
            assert!(n["dist"].is_number(), "{n}");
        }
    }
    // The same seed, the same queries.
    let (_, again) = recall(
        &api,
        "t",
        &json!({"num": 3, "top_k": 4, "seed": 9, "include_ground_truth": true,
                "filters": ["even", "Eq", true]}),
    )
    .await;
    assert_eq!(again["ground_truth"], b["ground_truth"]);
    // The default seed is 0.
    let truth = |seed: Option<u64>| {
        let mut body = json!({"num": 3, "top_k": 4, "include_ground_truth": true});
        if let Some(seed) = seed {
            body["seed"] = json!(seed);
        }
        let api = &api;
        async move { recall(api, "t", &body).await.1["ground_truth"].clone() }
    };
    let zero = truth(Some(0)).await;
    assert_eq!(truth(None).await, zero);
    assert_ne!(
        truth(Some(1)).await,
        zero,
        "another seed samples other queries"
    );
    // A filter nothing satisfies.
    let (s, b) = recall(&api, "t", &json!({"filters": ["even", "Eq", 7]})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (
            &b["avg_recall"],
            &b["avg_ann_count"],
            &b["avg_exhaustive_count"]
        ),
        (&Value::Null, &json!(0.0), &json!(0.0)),
        "{b}"
    );

    // Refusals.
    for body in [
        json!({"num": 0}),
        json!({"num": 26}),
        json!({"top_k": 0}),
        json!({"top_k": 101}),
        json!({"seed": -1}),
        json!({"seed": 1.5}),
        json!({"bogus": 1}),
        json!({"filters": ["even", "Nope", 1]}),
    ] {
        let (s, b) = recall(&api, "t", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}: {b}");
    }
    let (s, b) = recall(&api, "absent", &json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}
