//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `branch_from_namespace` and `copy_from_namespace` over the API (M16): the wire, what a
//! branch holds, and what is refused.

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

type A = Arc<Api<MemoryStore>>;

fn api() -> A {
    Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap()
}

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "48")
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

async fn put(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    send(api, "PUT", &format!("/v1/indexes/{index}/documents"), body).await
}

async fn ids(api: &A, index: &str) -> Vec<String> {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 1000, "include_attributes": true,
                   "filters": ["n", "Gte", 0]});
    let (s, b) = send(api, "POST", &format!("/v1/indexes/{index}/query"), &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_branch_holds_every_acknowledged_durable_write() {
    let a = api();
    let docs: Vec<Value> = (0..20)
        .map(|i| json!({"id": format!("d{i:02}"), "vector": [1.0, 0.5], "attributes": {"n": i}}))
        .collect();
    let (s, b) = put(
        &a,
        "src",
        &json!({"durability": "durable", "documents": docs}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // Unfolded until the branch: the branch folds the tenant first.
    let (s, b) = put(
        &a,
        "src",
        &json!({"durability": "durable", "deletes": ["d03"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for op in ["branch_from_namespace", "copy_from_namespace"] {
        let dest = format!("{op}-dest");
        let (s, b) = put(&a, &dest, &json!({op: "src"})).await;
        assert_eq!(s, StatusCode::OK, "{op}: {b}");
        assert!(b["epoch"].as_u64().unwrap() > 0, "{b}");
        // A session token that saw the commit: its epoch, as the M11 layout places it.
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(b["session"].as_str().unwrap())
            .unwrap();
        let saw = u64::from_le_bytes(token[17..25].try_into().unwrap());
        assert_eq!(saw, b["epoch"].as_u64().unwrap(), "{b}");
        assert_eq!(ids(&a, &dest).await, ids(&a, "src").await, "{op}");
        assert!(!ids(&a, &dest).await.contains(&"d03".to_owned()));
        // The schema too.
        let (_, stats) = send(&a, "GET", &format!("/v1/indexes/{dest}"), &json!({})).await;
        assert_eq!(stats["schema"]["dims"], 2, "{stats}");
    }
}

#[tokio::test]
async fn what_a_branch_cannot_mean_is_refused() {
    let a = api();
    let doc = json!({"id": "x", "vector": [1.0, 0.5], "attributes": {"n": 1}});
    for index in ["src", "taken"] {
        let (s, b) = put(
            &a,
            index,
            &json!({"durability": "durable", "documents": [doc]}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    let (s, _) = send(&a, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = send(&a, "DELETE", "/v1/indexes/taken", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for (dest, body, why) in [
        (
            "src2",
            json!({"branch_from_namespace": "nope"}),
            "does not exist",
        ),
        ("taken", json!({"branch_from_namespace": "src"}), "dropped"),
        ("src", json!({"branch_from_namespace": "src"}), "itself"),
        (
            "bad%20name",
            json!({"branch_from_namespace": "src"}),
            "name",
        ),
        ("..", json!({"branch_from_namespace": "src"}), "name"),
        (
            "src3",
            json!({"branch_from_namespace": "src", "documents": [doc]}),
            "alone",
        ),
        (
            "src4",
            json!({"branch_from_namespace": "src", "durability": "durable"}),
            "alone",
        ),
        (
            "src5",
            json!({"branch_from_namespace": "src", "copy_from_namespace": "src"}),
            "alone",
        ),
    ] {
        let (s, b) = put(&a, dest, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{dest} {body}: {b}");
        let msg = b["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains(why),
            "{dest} {body}: {msg:?} does not name {why:?}"
        );
    }
    // A live destination.
    let (s, _) = put(&a, "src6", &json!({"branch_from_namespace": "src"})).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = put(&a, "src6", &json!({"branch_from_namespace": "src"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["error"]["message"].as_str().unwrap().contains("exists"),
        "{b}"
    );
}
