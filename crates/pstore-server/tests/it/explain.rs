//! M165: `"explain": true` on a ranked query reports the plan it ran, at no cost.
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

const TENANT: u64 = 165;

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

async fn query(api: &A, body: &Value) -> (StatusCode, Value) {
    send(api, "POST", "/v1/indexes/t/query", body).await
}

async fn fold(api: &A) {
    api.fold_due(&FoldPolicy {
        period: std::time::Duration::from_secs(1),
        age: std::time::Duration::ZERO,
        bytes: 1,
    })
    .await;
}

fn doc(i: usize) -> Value {
    let x = i as f64;
    json!({"id": format!("d{i:04}"), "vector": [x.sin(), x.cos(), (x * 0.37).sin(), 1.0],
           "text": format!("common word{}", i % 7), "attributes": {"n": i}})
}

/// Two clustered segments of 200 rows, each of 10 lists. No boundary replicas, so a
/// segment's index rows are its rows.
async fn index() -> A {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    api.set_index_params_for_test(pstore_index::cluster::Params {
        target_list_size: 20,
        exact_scan_threshold: 8,
        replicas: 0,
        ..pstore_index::cluster::Params::default()
    });
    for batch in 0..2 {
        let docs: Vec<Value> = (batch * 200..batch * 200 + 200).map(doc).collect();
        let (s, b) = send(
            &api,
            "PUT",
            "/v1/indexes/t/documents",
            &json!({"documents": docs, "durability": "durable"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        fold(&api).await;
    }
    api
}

#[tokio::test]
async fn explain_reports_the_plan() {
    let api = index().await;
    let v = [0.3, 0.9, -0.2, 1.0];

    // Criterion 1: a default dense query, two clustered segments, eight lists of each.
    let plain = json!({"vector": v, "top_k": 10});
    let mut asked = plain.clone();
    asked["explain"] = json!(true);
    let (s, without) = query(&api, &plain).await;
    assert_eq!(s, StatusCode::OK, "{without}");
    let (s, with) = query(&api, &asked).await;
    assert_eq!(s, StatusCode::OK, "{with}");
    assert_eq!(
        with["meta"]["explain"],
        json!({"segments": 2, "shadow": 0, "remote_segments": 0, "filter": false,
               "fusion": "rrf", "legs": [{
                   "kind": "dense", "field": "vector", "limit": 10, "mode": "limited",
                   "requested": 20, "candidates": 20, "kept": 20,
                   "approximate_segments": 2, "exact_segments": 0, "probes": 16}]})
    );
    // Criterion 6: the same answer at the same cost, and no key unasked.
    assert_eq!(with["results"], without["results"]);
    assert_eq!(with["meta"]["cost"], without["meta"]["cost"]);
    assert!(without["meta"].get("explain").is_none(), "{without}");
    let mut off = plain.clone();
    off["explain"] = json!(false);
    let (_, off) = query(&api, &off).await;
    assert!(off["meta"].get("explain").is_none(), "{off}");

    // `exact`: both segments scanned, none probed.
    let (_, exact) = query(
        &api,
        &json!({"vector": v, "top_k": 10, "exact": true, "explain": true}),
    )
    .await;
    let leg = &exact["meta"]["explain"]["legs"][0];
    assert_eq!(
        (
            &leg["approximate_segments"],
            &leg["exact_segments"],
            &leg["probes"]
        ),
        (&json!(0), &json!(2), &json!(0)),
        "{exact}"
    );

    // Criterion 2 and 4: a filter makes both legs exhaustive; legs in request order.
    let (s, f) = query(
        &api,
        &json!({"vector": v, "text": "word3", "top_k": 10, "filters": ["n", "Lt", 250],
                "explain": true}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{f}");
    let x = &f["meta"]["explain"];
    assert_eq!(x["filter"], json!(true));
    let kinds: Vec<&Value> = x["legs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| &l["kind"])
        .collect();
    assert_eq!(kinds, [&json!("dense"), &json!("text")], "{x}");
    for leg in x["legs"].as_array().unwrap() {
        assert_eq!(leg["mode"], json!("exhaustive"), "{leg}");
        assert_eq!(leg["exhaustive_because"], json!("filter"), "{leg}");
        assert_eq!(leg["requested"], json!(400), "{leg}");
    }
    // The text leg: `word3` is in every seventh document, and the mask keeps those below 250,
    // up to each segment's limit of 10.
    let below = |lo: usize, hi: usize| (lo..hi).filter(|i| i % 7 == 3 && *i < 250).count();
    assert_eq!(
        x["legs"][1]["candidates"],
        json!((0..400).filter(|i| i % 7 == 3).count())
    );
    assert_eq!(
        x["legs"][1]["kept"],
        json!(below(0, 200).min(10) + below(200, 400).min(10))
    );
    assert!(x["legs"][1].get("probes").is_none(), "{x}");

    // Criterion 3: an unfolded overwrite here widens each folded segment by one.
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/t/documents",
        &json!({"documents": [doc(5)], "durability": "durable"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (_, w) = query(&api, &asked).await;
    let x = &w["meta"]["explain"];
    assert_eq!(
        (&x["segments"], &x["shadow"]),
        (&json!(2), &json!(1)),
        "{x}"
    );
    assert_eq!(x["legs"][0]["mode"], json!("widened"), "{x}");
    assert_eq!(x["legs"][0]["requested"], json!(22), "{x}");
}

#[tokio::test]
async fn explain_refusals() {
    let api = index().await;
    let v = [0.3, 0.9, -0.2, 1.0];
    for (name, body) in [
        (
            "a string",
            json!({"vector": v, "top_k": 10, "explain": "yes"}),
        ),
        (
            "rank_by",
            json!({"rank_by": ["id", "asc"], "top_k": 10, "explain": true}),
        ),
        (
            "aggregate_by",
            json!({"aggregate_by": {"n": ["Count", "id"]}, "explain": true}),
        ),
        (
            "a multi-query",
            json!({"queries": [{"vector": v, "top_k": 5, "explain": true}]}),
        ),
    ] {
        let (s, b) = query(&api, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{name}: {b}");
    }
}
