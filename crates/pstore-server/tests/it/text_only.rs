//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Documents with no vector (M170): written, filtered, ranked, projected, patched and read
//! back in id order across a restart, and refused where a vector is asked of them.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

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
        .header("x-pstore-tenant", "19")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn write(api: &Arc<Api<MemoryStore>>, index: &str, body: &Value) -> (StatusCode, Value) {
    let uri = format!("/v1/indexes/{index}/documents");
    send(api, request("PUT", &uri, Some(body))).await
}

async fn query(api: &Arc<Api<MemoryStore>>, index: &str, body: &Value) -> (StatusCode, Value) {
    let uri = format!("/v1/indexes/{index}/query");
    send(api, request("POST", &uri, Some(body))).await
}

async fn fold(api: &Arc<Api<MemoryStore>>) {
    let (s, b) = send(api, request("POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn ids(b: &Value) -> Vec<String> {
    b["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results: {b}"))
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn api(store: &Accounted<MemoryStore>) -> Arc<Api<MemoryStore>> {
    Api::unsecured(store.clone(), LaneId(1)).unwrap()
}

/// Three vector-free documents, durable and folded.
async fn seeded(store: &Accounted<MemoryStore>) -> Arc<Api<MemoryStore>> {
    let a = api(store);
    let docs = json!({"durability": "durable", "documents": [
        {"id": "a", "text": "the quick brown fox", "attributes": {"n": 1, "lang": "en"}},
        {"id": "b", "text": "a lazy brown dog", "attributes": {"n": 2, "lang": "en"}},
        {"id": "c", "text": "le renard brun", "attributes": {"n": 3, "lang": "fr"}},
    ]});
    let (s, b) = write(&a, "t", &docs).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&a).await;
    a
}

#[tokio::test]
async fn a_vector_free_document_is_written_filtered_ranked_and_projected() {
    let store = Accounted::new(MemoryStore::new());
    let a = seeded(&store).await;

    let (s, b) = query(&a, "t", &json!({"text": "brown", "top_k": 10})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut got = ids(&b);
    got.sort_unstable();
    assert_eq!(got, ["a", "b"], "BM25 over vector-free rows: {b}");

    let f = json!({"text": "brown", "top_k": 10, "filters": ["n", "Gte", 2]});
    let (s, b) = query(&a, "t", &f).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ids(&b), ["b"], "a filter over vector-free rows: {b}");

    let r = json!({"rank_by": ["n", "desc"], "top_k": 10, "filters": ["lang", "Eq", "en"],
                   "include_attributes": true});
    let (s, b) = query(&a, "t", &r).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ids(&b), ["b", "a"], "an ordered, filtered read: {b}");
    assert_eq!(b["results"][0]["attributes"]["lang"], "en", "{b}");
    assert_eq!(b["results"][0]["attributes"]["n"], 2, "{b}");
}

#[tokio::test]
async fn a_vector_free_document_is_patched_and_read_back_after_a_restart() {
    let store = Accounted::new(MemoryStore::new());
    let a = seeded(&store).await;
    let p = json!({"durability": "durable", "patch_rows": [{"id": "a", "attributes": {"n": 9}}]});
    let (s, b) = write(&a, "t", &p).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&a).await;
    drop(a);

    let after = api(&store);
    let page = |after: Value| {
        let mut q = json!({"rank_by": ["id", "asc"], "top_k": 2, "include_attributes": true});
        if !after.is_null() {
            q["filters"] = json!(["id", "Gt", after]);
        }
        q
    };
    let (s, first) = query(&after, "t", &page(Value::Null)).await;
    assert_eq!(s, StatusCode::OK, "{first}");
    assert_eq!(ids(&first), ["a", "b"], "{first}");
    assert_eq!(
        first["results"][0]["attributes"]["n"], 9,
        "the patch: {first}"
    );
    let (s, rest) = query(&after, "t", &page(json!("b"))).await;
    assert_eq!(s, StatusCode::OK, "{rest}");
    assert_eq!(ids(&rest), ["c"], "{rest}");
    assert_eq!(rest["results"][0]["attributes"]["lang"], "fr", "{rest}");

    let (s, b) = query(&after, "t", &json!({"text": "fox", "top_k": 10})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ids(&b), ["a"], "BM25 after the restart: {b}");
}

#[tokio::test]
async fn a_vector_query_against_a_vector_free_index_is_refused_by_name() {
    let store = Accounted::new(MemoryStore::new());
    let a = seeded(&store).await;
    for q in [
        json!({"vector": [1.0, 0.5], "top_k": 3}),
        json!({"vector": [1.0, 0.5], "text": "brown", "top_k": 3}),
        json!({"multi": {"field": "late", "vectors": [[1.0, 0.5]]}, "top_k": 3}),
        json!({"vector": [1.0, 0.5], "exact": true, "top_k": 3}),
        json!({"vector": [1.0, 0.5], "top_k": 3, "as_of": 1}),
    ] {
        let (s, b) = query(&a, "t", &q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{q} was not refused: {b}");
        assert_eq!(b["error"]["code"], "schema_conflict", "{q}: {b}");
    }
}

#[tokio::test]
async fn an_index_takes_vectors_or_none_never_both() {
    let store = Accounted::new(MemoryStore::new());
    let a = seeded(&store).await;
    let vectored = json!({"durability": "durable", "documents": [
        {"id": "v", "vector": [1.0, 0.5]}]});
    let (s, b) = write(&a, "t", &vectored).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "a vector into a vector-free index: {b}"
    );
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");

    let (s, b) = write(&a, "d", &vectored).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&a).await;
    let bare = json!({"durability": "durable", "documents": [{"id": "w", "text": "x"}]});
    let (s, b) = write(&a, "d", &bare).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "a vector-free row into a dense index: {b}"
    );
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");

    let mixed = json!({"documents": [
        {"id": "x", "vector": [1.0, 0.5]}, {"id": "y", "text": "y"}]});
    let (s, b) = write(&a, "m", &mixed).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a mixed batch: {b}");
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    let said = b["error"]["message"].as_str().unwrap();
    assert!(
        said.contains("document y has no vector and the batch has 2 dimensions"),
        "refused at the door, by name: {b}"
    );
}

#[tokio::test]
async fn what_a_vector_free_document_cannot_carry_is_refused() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store);
    for body in [
        json!({"documents": [{"id": "a", "vector": []}]}),
        json!({"documents": [{"id": "a", "vectors": {"late": [[1.0]]}}]}),
        json!({"distance_metric": "cosine_distance", "documents": [{"id": "a", "text": "x"}]}),
        json!({"distance_metric": "euclidean_squared", "documents": [{"id": "a", "text": "x"}]}),
        json!({"distance_metric": "dot_product", "documents": [{"id": "a", "text": "x"}]}),
    ] {
        let (s, b) = write(&a, "t", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} was accepted: {b}");
    }
}

#[tokio::test]
async fn an_index_first_folded_with_deletes_alone_still_takes_vectors() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store);
    let gone = json!({"durability": "durable", "deletes": ["z"]});
    let (s, b) = write(&a, "e", &gone).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&a).await;
    let vectored = json!({"durability": "durable", "documents": [
        {"id": "v", "vector": [1.0, 0.5]}]});
    let (s, b) = write(&a, "e", &vectored).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&a).await;
    let (s, b) = query(&a, "e", &json!({"vector": [1.0, 0.5], "top_k": 3})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ids(&b), ["v"], "{b}");
}

#[tokio::test]
async fn a_vector_query_against_unfolded_vector_free_rows_is_refused_by_name() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store);
    let bare = json!({"documents": [{"id": "a", "text": "the fox"}]});
    let (s, b) = write(&a, "u", &bare).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = query(&a, "u", &json!({"text": "fox", "top_k": 3})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ids(&b), ["a"], "{b}");
    let (s, b) = query(&a, "u", &json!({"vector": [1.0, 0.5], "top_k": 3})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
}
