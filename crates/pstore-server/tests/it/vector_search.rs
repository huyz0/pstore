//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M198: an index answers, for each dense field, how many of its folded rows a query on the
//! field searches by clusters, exactly, or as unknown.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, MemoryStore};
use pstore_engine::Head;
use pstore_server::Api;
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

const TENANT: u128 = 198;

async fn send(
    api: &Arc<Api<MemoryStore>>,
    method: &str,
    uri: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT.to_string())
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(r).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn write(api: &Arc<Api<MemoryStore>>, ids: std::ops::Range<usize>) {
    let docs: Vec<Value> = ids
        .map(|i| {
            let v = [i as f32, 1.0, -0.5];
            let mut d = json!({"id": format!("d{i:04}"), "vector": v,
                "vectors": {"a": [[1.0, i as f32]]}});
            if i % 3 != 0 {
                d["vectors"]["b"] = json!([[0.5, 0.25, i as f32, 2.0]]);
            }
            d
        })
        .collect();
    let body = json!({"durability": "durable", "documents": docs});
    let (s, b) = send(api, "PUT", "/v1/indexes/v/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold(api: &Arc<Api<MemoryStore>>) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn summary(api: &Arc<Api<MemoryStore>>) -> Value {
    let (s, b) = send(api, "GET", "/v1/indexes/v", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn each(approximate: u64, exact: u64, unknown: u64) -> Value {
    let one = json!({"approximate_rows": approximate, "exact_rows": exact,
        "unknown_rows": unknown});
    json!({"vector": one, "a": one, "b": one})
}

#[tokio::test]
async fn each_dense_fields_rows_are_answered_by_how_a_query_searches_them() {
    // Criteria 1 and 2, through the server: small segments are scanned exactly.
    let store = MemoryStore::new();
    let api = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    // Before the first fold, nothing is answered.
    write(&api, 0..40).await;
    let meta = summary(&api).await;
    assert!(
        meta.get("vector_search").is_none_or(Value::is_null),
        "{meta}"
    );
    fold(&api).await;
    write(&api, 40..100).await;
    fold(&api).await;
    // A deleted row is no live row, as `documents` counts it.
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/v/documents",
        &json!({"durability": "durable", "deletes": ["d0001"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&api).await;
    // Unfolded rows are in no count.
    write(&api, 100..110).await;
    let meta = summary(&api).await;
    assert_eq!(meta["documents"], 99, "{meta}");
    assert_eq!(meta["vector_search"], each(0, 99, 0), "{meta}");

    // From a fresh process, and after a compaction merges the two segments.
    let fresh = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
    assert_eq!(summary(&fresh).await["vector_search"], each(0, 99, 0));
    let tenant = TenantId(TENANT);
    let view = Arc::new(Accounted::new(store.clone()).as_tenant(tenant));
    let engine = pstore_engine::Engine::new(Arc::clone(&view), tenant, LaneId(9));
    assert!(
        engine.compact("v").await.unwrap().is_some(),
        "nothing compacted"
    );
    let again = Api::unsecured(Accounted::new(store.clone()), LaneId(3)).unwrap();
    let meta = summary(&again).await;
    assert_eq!(meta["segments"], 1, "{meta}");
    assert_eq!(meta["vector_search"], each(0, 99, 0), "{meta}");

    // A HEAD that records no set, as one written before M198: every row is unknown.
    let key = Head::key(tenant);
    let mut head = Head::decode(&view.get(&key).await.unwrap()).unwrap();
    for r in head.indexes.values_mut().flatten() {
        assert!(r.ann.is_some(), "{r:?}");
        r.ann = None;
    }
    view.put(&key, Bytes::from(head.encode())).await.unwrap();
    let old = Api::unsecured(Accounted::new(store.clone()), LaneId(4)).unwrap();
    assert_eq!(summary(&old).await["vector_search"], each(0, 0, 99));
}

#[tokio::test]
async fn an_index_with_no_vector_answers_no_dense_field() {
    // M170's vector-free index records width 0: `vector` is no field of it.
    let store = MemoryStore::new();
    let api = Api::unsecured(Accounted::new(store), LaneId(1)).unwrap();
    let body = json!({"durability": "durable", "documents": [{"id": "t", "text": "hello"}]});
    let (s, b) = send(&api, "PUT", "/v1/indexes/v/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&api).await;
    let meta = summary(&api).await;
    assert_eq!(meta["schema"]["dims"], 0, "{meta}");
    assert_eq!(meta["vector_search"], json!({}), "{meta}");
}

#[tokio::test]
async fn a_field_a_segment_searches_by_clusters_is_answered_approximate() {
    // Criterion 1's server half: HEAD's sets, as the seal writes them, reach the answer.
    let store = MemoryStore::new();
    let api = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    write(&api, 0..30).await;
    fold(&api).await;
    write(&api, 30..50).await;
    fold(&api).await;
    // Stand in for a big segment: the first segment's set names `vector` and `a`.
    let tenant = TenantId(TENANT);
    let view = Accounted::new(store.clone()).as_tenant(tenant);
    let key = Head::key(tenant);
    let mut head = Head::decode(&view.get(&key).await.unwrap()).unwrap();
    let refs = head.indexes.get_mut("v").unwrap();
    assert_eq!(refs.len(), 2);
    refs[0].ann = Some(["vector".to_owned(), "a".to_owned()].into());
    refs[1].ann = None;
    view.put(&key, Bytes::from(head.encode())).await.unwrap();
    let fresh = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
    let one =
        |a: u64, e: u64, u: u64| json!({"approximate_rows": a, "exact_rows": e, "unknown_rows": u});
    assert_eq!(
        summary(&fresh).await["vector_search"],
        json!({"vector": one(30, 0, 20), "a": one(30, 0, 20), "b": one(0, 30, 20)})
    );
}
