//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `offset` on a relevance ranking through the API — M163. Every page is checked against
//! the same query at `top_k = offset + top_k` without `offset`, sliced.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_server::Api;
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

async fn send<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    method: &str,
    uri: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "23")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn query<S: BlobStore + 'static>(api: &Arc<Api<S>>, body: &Value) -> Value {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
    b
}

fn doc(i: usize) -> Value {
    let x = i as f32;
    let mut attrs = json!({"pop": (i * 37 % 101) as i64});
    if i % 3 != 2 {
        attrs["g"] = json!(i % 4);
    }
    json!({"id": format!("d{i:02}"), "vector": [x.cos(), x.sin(), 1.0, 0.5],
           "text": format!("common word{} word{}", i % 5, i % 3), "attributes": attrs})
}

async fn corpus<S: BlobStore + 'static>(store: Accounted<S>) -> Arc<Api<S>> {
    let api = Api::unsecured(store, LaneId(1)).unwrap();
    let docs: Vec<Value> = (0..40).map(doc).collect();
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": docs})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(&api, "POST", "/v1/admin/fold", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    api
}

fn with(base: &Value, extra: Value) -> Value {
    let mut q = base.clone();
    for (k, x) in extra.as_object().unwrap() {
        q[k] = x.clone();
    }
    q
}

/// The rows of an answer as compared: id, score and `$dist`.
fn rows(b: &Value) -> Vec<Value> {
    b["results"]
        .as_array()
        .unwrap_or_else(|| panic!("{b}"))
        .iter()
        .map(|r| json!([r["id"], r["score"], r["$dist"]]))
        .collect()
}

/// The page at `o` of `top_k`, and the same query at `o + top_k`, sliced.
async fn page_and_slice<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    base: &Value,
    o: usize,
    top_k: usize,
) -> (Vec<Value>, Vec<Value>) {
    let page = query(api, &with(base, json!({"top_k": top_k, "offset": o}))).await;
    let deep = query(api, &with(base, json!({"top_k": o + top_k}))).await;
    let all = rows(&deep);
    (rows(&page), all.into_iter().skip(o).collect())
}

fn bases() -> Vec<Value> {
    let v = json!([1.0, 0.2, 1.0, 0.5]);
    vec![
        json!({"vector": v}),
        json!({"text": "common word2"}),
        json!({"vector": v, "text": "common word1"}),
        json!({"text": ["common word2", "word1"], "fusion": {"sum": {}}}),
        json!({"vector": v, "exact": true}),
    ]
}

#[tokio::test]
async fn a_page_is_the_slice_of_the_ranking_at_its_depth() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    for base in bases() {
        for o in [0, 1, 3, 7] {
            let (page, slice) = page_and_slice(&api, &base, o, 4).await;
            assert_eq!(page.len(), 4, "{base} at {o}");
            assert_eq!(page, slice, "{base} at {o}");
        }
    }
}

#[tokio::test]
async fn exact_pages_concatenate_to_one_ranking() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    let base = json!({"vector": [1.0, 0.2, 1.0, 0.5], "exact": true});
    let mut pages = Vec::new();
    for o in [0, 4, 8] {
        pages.extend(rows(
            &query(&api, &with(&base, json!({"top_k": 4, "offset": o}))).await,
        ));
    }
    assert_eq!(
        pages,
        rows(&query(&api, &with(&base, json!({"top_k": 12}))).await)
    );
}

#[tokio::test]
async fn a_boosted_or_group_limited_page_is_a_slice_of_its_ranking() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    let v = json!([1.0, 0.2, 1.0, 0.5]);
    let terms = json!([{"attr": "pop", "fn": "linear", "weight": 0.01}]);
    for base in [
        json!({"vector": v, "boost": {"window": 30, "terms": terms}}),
        json!({"vector": v, "boost": {"terms": terms}}),
        json!({"vector": v, "limit_per": {"attributes": ["g"], "limit": 2, "window": 30}}),
        json!({"vector": v, "limit_per": {"attributes": ["g"], "limit": 2}}),
    ] {
        for o in [1, 5] {
            let (page, slice) = page_and_slice(&api, &base, o, 4).await;
            assert_eq!(page, slice, "{base} at {o}");
        }
    }
}

#[tokio::test]
async fn unfolded_hits_count_the_returned_rows() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    // Rewrites of every folded id, unfolded in the querying process, then a new id.
    let docs: Vec<Value> = (0..40).filter(|i| i % 2 == 0).map(doc).collect();
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"documents": docs})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let base = json!({"vector": [1.0, 0.2, 1.0, 0.5], "exact": true});
    let deep = query(&api, &with(&base, json!({"top_k": 10}))).await;
    let page = query(&api, &with(&base, json!({"top_k": 4, "offset": 6}))).await;
    let fresh = |b: &Value| {
        b["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["id"].as_str().unwrap()[1..].parse::<usize>().unwrap() % 2 == 0)
            .count()
    };
    assert_ne!(
        fresh(&page),
        deep["meta"]["unfolded_hits"].as_u64().unwrap() as usize,
        "the fixture proves nothing"
    );
    assert_eq!(page["meta"]["unfolded_hits"], fresh(&page), "{page}");
}

#[tokio::test]
async fn what_cannot_mean_a_page_is_refused_before_any_blob_request() {
    let store = Accounted::new(MemoryStore::new());
    let api = corpus(store.clone()).await;
    let spent =
        || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(23), c));
    let v = json!([1.0, 0.2, 1.0, 0.5]);
    let terms = json!([{"attr": "pop", "fn": "linear", "weight": 0.01}]);
    for body in [
        json!({"vector": v, "offset": 1, "diversify": {"lambda": 0.5}}),
        json!({"aggregate_by": {"n": ["Count", "pop"]}, "offset": 1}),
        json!({"vector": v, "top_k": 10_000, "offset": 1}),
        json!({"vector": v, "top_k": 5, "offset": 6, "boost": {"window": 10, "terms": terms}}),
        json!({"vector": v, "top_k": 5, "offset": 6, "limit_per": {"attributes": ["g"], "limit": 1, "window": 10}}),
        // An exact-rerank multi leg past depth 1,000: it panicked in `clamp` before M163.
        json!({"multi": {"field": "late", "vectors": [[1.0, 0.0]], "mode": "fde", "rerank": "exact"},
               "top_k": 10, "offset": 995}),
        json!({"multi": {"field": "late", "vectors": [[1.0, 0.0]], "mode": "fde", "rerank": "exact"},
               "top_k": 1001}),
    ] {
        let before = spent();
        let (s, b) = send(&api, "POST", "/v1/indexes/docs/query", Some(&body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
        assert_eq!(b["error"]["code"], "bad_request", "{body} -> {b}");
        assert_eq!(before, spent(), "{body}: the refusal made blob requests");
    }
    // The bounds themselves are allowed.
    for body in [
        json!({"vector": v, "top_k": 9_999, "offset": 1}),
        json!({"vector": v, "top_k": 5, "offset": 5, "boost": {"window": 10, "terms": terms}}),
    ] {
        query(&api, &body).await;
    }
}

#[tokio::test]
async fn facets_beside_a_page_answer_as_beside_the_first() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    let base = json!({"vector": [1.0, 0.2, 1.0, 0.5], "top_k": 4,
                      "facets": {"aggregate_by": {"n": ["Count", "pop"]}, "group_by": ["g"]}});
    let first = query(&api, &base).await;
    let paged = query(&api, &with(&base, json!({"offset": 5}))).await;
    assert!(
        first["facets"].is_object() || first["facets"].is_array(),
        "{first}"
    );
    assert_eq!(first["facets"], paged["facets"]);
    assert_ne!(rows(&first), rows(&paged), "the fixture proves nothing");
}
