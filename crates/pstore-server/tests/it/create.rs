//! M147: `PUT /v1/indexes/{index}` creates an index with no rows.
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
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_cache::CacheCore;
use pstore_server::{Api, Auth, FoldPolicy, RequestLimits};
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const TENANT: u64 = 41;

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

fn cached(store: &Accounted<MemoryStore>, lane: u64) -> A {
    Api::with_cache_unsecured(
        store.clone(),
        LaneId(lane),
        Arc::new(CacheCore::memory(16 << 20)),
    )
    .unwrap()
}

async fn create(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    send(api, "PUT", &format!("/v1/indexes/{index}"), body).await
}

async fn write(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    send(api, "PUT", &format!("/v1/indexes/{index}/documents"), body).await
}

async fn query(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    send(api, "POST", &format!("/v1/indexes/{index}/query"), body).await
}

async fn fold(api: &A) {
    api.fold_due(&FoldPolicy {
        period: std::time::Duration::from_secs(1),
        age: std::time::Duration::ZERO,
        bytes: 1,
    })
    .await;
}

fn puts(store: &Accounted<MemoryStore>) -> u64 {
    store.count(TenantId(u128::from(TENANT)), OpClass::Write)
}

fn listed(b: &Value) -> Vec<String> {
    b["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap().to_owned())
        .collect()
}

/// Every read route answers an empty index as existing and empty.
async fn reads_as_empty(api: &A, index: &str, dims: u64, metric: &str) {
    let (s, b) = send(api, "GET", "/v1/indexes", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(listed(&b).contains(&index.to_owned()), "{b}");
    let (s, b) = send(api, "GET", &format!("/v1/indexes/{index}"), &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["segments"].as_u64(), b["documents"].as_u64()),
        (Some(0), Some(0)),
        "{b}"
    );
    assert_eq!(b["schema"]["dims"], dims, "{b}");
    assert_eq!(b["schema"]["distance_metric"], metric, "{b}");
    let vector: Vec<f64> = (0..dims).map(|_| 1.0).collect();
    let (s, b) = query(api, index, &json!({"vector": vector, "top_k": 5})).await;
    assert_eq!(s, StatusCode::OK, "relevance: {b}");
    assert_eq!(b["results"], json!([]), "{b}");
    let (s, b) = query(api, index, &json!({"rank_by": ["n", "asc"], "top_k": 5})).await;
    assert_eq!(s, StatusCode::OK, "rank_by: {b}");
    assert_eq!(b["results"], json!([]), "{b}");
    let (s, b) = query(
        api,
        index,
        &json!({"aggregate_by": {"n": ["Count", "id"]}, "top_k": 5}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "aggregate: {b}");
    assert_eq!(b["aggregations"]["n"], 0, "aggregate: {b}");
    let (s, b) = send(
        api,
        "POST",
        &format!("/v1/indexes/{index}/warm"),
        &Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "warm: {b}");
    assert_eq!(b["segments"], 0, "{b}");
}

#[tokio::test]
async fn created_index_survives_restart() {
    let store = Accounted::new(MemoryStore::new());
    let api = cached(&store, 1);
    let (s, b) = create(
        &api,
        "fresh",
        &json!({"dims": 3, "distance_metric": "cosine_distance"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let epoch = b["epoch"].as_u64().unwrap();
    assert!(epoch > 0, "{b}");
    reads_as_empty(&api, "fresh", 3, "cosine_distance").await;
    // A restart: another process over the same store.
    let again = cached(&store, 2);
    reads_as_empty(&again, "fresh", 3, "cosine_distance").await;
    // Absent at its creation epoch: `as_of` reads only an index with a segment.
    let (s, b) = query(
        &again,
        "fresh",
        &json!({"vector": [1, 1, 1], "top_k": 1, "as_of": epoch}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn create_refusals_touch_nothing() {
    let store = Accounted::new(MemoryStore::new());
    let limits = RequestLimits::builder().array_elements(64).build().unwrap();
    let api = Api::with_limits(
        store.clone(),
        LaneId(1),
        None,
        Auth::from_vars(|k| (k == "PSTORE_ALLOW_UNAUTHENTICATED").then(|| "1".to_owned())).unwrap(),
        limits,
    )
    .unwrap();
    // Each kind of existing name: segments, rows held unfolded, rejects only.
    let (s, b) = write(
        &api,
        "folded",
        &json!({"durability": "durable",
        "documents": [{"id": "a", "vector": [1.0]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&api).await;
    let (s, b) = write(
        &api,
        "held",
        &json!({"documents": [{"id": "a", "vector": [1.0]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = create(&api, "made", &json!({"dims": 1})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let before = puts(&store);
    for (index, body, status, code) in [
        (
            "folded",
            json!({"dims": 1}),
            StatusCode::CONFLICT,
            "index_exists",
        ),
        (
            "held",
            json!({"dims": 1}),
            StatusCode::CONFLICT,
            "index_exists",
        ),
        (
            "made",
            json!({"dims": 2}),
            StatusCode::CONFLICT,
            "index_exists",
        ),
        (
            "x",
            json!({"dims": 0}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "x",
            json!({"dims": 65}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "x",
            json!({"dims": -1}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        ("x", json!({}), StatusCode::BAD_REQUEST, "bad_request"),
        (
            "x",
            json!({"dims": 2, "distance_metric": "manhattan"}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "x",
            json!({"dims": 2, "text_field": "t"}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        ("x", json!([2]), StatusCode::BAD_REQUEST, "bad_request"),
        (
            "..",
            json!({"dims": 2}),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
    ] {
        let (s, b) = create(&api, index, &body).await;
        assert_eq!(s, status, "{index} {body}: {b}");
        assert_eq!(b["error"]["code"], code, "{index} {body}: {b}");
    }
    assert_eq!(puts(&store), before, "a refusal wrote");
    // At the limit is admitted.
    let (s, b) = create(&api, "wide", &json!({"dims": 64})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn first_write_follows_the_declared_schema() {
    let store = Accounted::new(MemoryStore::new());
    let api = cached(&store, 1);
    let (s, b) = create(
        &api,
        "c",
        &json!({"dims": 2, "distance_metric": "cosine_distance"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let before = puts(&store);
    // No read between the create and these: this process's own caches must know.
    for (body, why, code) in [
        (
            json!({"distance_metric": "cosine_distance",
            "documents": [{"id": "a", "vector": [1.0, 0.0, 0.0]}]}),
            "another width",
            "schema_conflict",
        ),
        (
            json!({"documents": [{"id": "a", "vector": [1.0, 0.0]}]}),
            "the default metric",
            "schema_conflict",
        ),
        (
            json!({"distance_metric": "cosine_distance", "schema": {"text": {"type": "string",
            "full_text_search": {"language": "french"}}},
            "documents": [{"id": "a", "vector": [1.0, 0.0], "text": "bonjour"}]}),
            "another analyzer",
            "schema_conflict",
        ),
    ] {
        let (s, b) = write(&api, "c", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{why}: {b}");
        assert_eq!(b["error"]["code"], code, "{why}: {b}");
    }
    assert_eq!(puts(&store), before, "a refused write wrote");
    let (s, b) = write(
        &api,
        "c",
        &json!({"durability": "durable", "distance_metric": "cosine_distance",
        "documents": [{"id": "a", "vector": [1.0, 0.0]}, {"id": "b", "vector": [0.0, 1.0]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&api).await;
    let (s, b) = send(&api, "GET", "/v1/indexes/c", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["segments"].as_u64(), b["documents"].as_u64()),
        (Some(1), Some(2)),
        "{b}"
    );
    let (s, b) = query(&api, "c", &json!({"vector": [3.0, 0.0], "top_k": 2})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // Cosine: 1 - cos, so 0 for the parallel row and 1 for the orthogonal one, whatever the
    // query's length; a dot product would score them -3 and 0.
    let r = &b["results"];
    assert_eq!(r[0]["id"], "a", "{b}");
    assert!(r[0]["$dist"].as_f64().unwrap().abs() < 1e-5, "{b}");
    assert!((r[1]["$dist"].as_f64().unwrap() - 1.0).abs() < 1e-5, "{b}");
}

#[tokio::test]
async fn a_fold_keeps_an_empty_index() {
    let store = Accounted::new(MemoryStore::new());
    let api = cached(&store, 1);
    let (s, b) = create(&api, "empty", &json!({"dims": 2})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = write(
        &api,
        "other",
        &json!({"durability": "durable",
        "documents": [{"id": "a", "vector": [1.0]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&api).await;
    reads_as_empty(&cached(&store, 2), "empty", 2, "dot_product").await;
}

#[tokio::test]
async fn an_empty_index_drops_and_its_name_is_reused() {
    let store = Accounted::new(MemoryStore::new());
    let api = cached(&store, 1);
    let (s, b) = create(&api, "re", &json!({"dims": 2})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(&api, "DELETE", "/v1/indexes/re", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(&api, "GET", "/v1/indexes", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(!listed(&b).contains(&"re".to_owned()), "{b}");
    let (s, b) = send(&api, "GET", "/v1/indexes/re", &Value::Null).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    let (s, b) = create(&api, "re", &json!({"dims": 3})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // Sent next, in the same process: the old width is refused, the new one admitted.
    let (s, b) = write(
        &api,
        "re",
        &json!({"documents": [{"id": "a", "vector": [1.0, 2.0]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, b) = write(
        &api,
        "re",
        &json!({"documents": [{"id": "a", "vector": [1.0, 2.0, 3.0]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn create_costs_one_read_and_one_cas() {
    let store = Accounted::new(MemoryStore::new());
    let t = TenantId(u128::from(TENANT));
    let before = [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(t, c));
    let (s, b) = create(&cached(&store, 1), "one", &json!({"dims": 4})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let after = [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(t, c));
    assert_eq!(
        [
            after[0] - before[0],
            after[1] - before[1],
            after[2] - before[2]
        ],
        [1, 1, 0],
        "HEAD GET, HEAD CAS, no LIST"
    );
    assert_eq!(b["cost"]["blob_reads"], 1, "{b}");
    assert_eq!(b["cost"]["blob_writes"], 1, "{b}");
}

/// M262: a create's `schema`, the map a write declares.
fn declared() -> Value {
    json!({
        "text": {"type": "string", "full_text_search": {"k1": 2.0, "stemming": true}},
        "title": {"type": "string", "full_text_search": {"language": "french"}},
        "sku": {"type": "string", "regex": true},
        "n": "uint64",
        "id": "uuid",
    })
}

const UUID: &str = "0b6fa4a6-6a0b-4c8b-9f43-6c3b8d2f7a11";

fn summary_schema(b: &Value) -> &Value {
    &b["schema"]
}

#[tokio::test]
async fn created_schema_is_recorded() {
    let store = Accounted::new(MemoryStore::new());
    let a = cached(&store, 1);
    let (s, b) = create(&a, "docs", &json!({"dims": 2, "schema": declared()})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for api in [&a, &cached(&store, 2)] {
        let (s, b) = send(api, "GET", "/v1/indexes/docs", &Value::Null).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let sc = summary_schema(&b);
        assert_eq!(sc["full_text_search"]["k1"], 2.0, "{b}");
        assert_eq!(sc["full_text_search"]["stemming"], true, "{b}");
        assert_eq!(sc["text_fields"]["title"]["language"], "french", "{b}");
        assert_eq!(sc["regex"], json!(["sku"]), "{b}");
        assert_eq!(sc["attributes"]["n"], "uint64", "{b}");
        assert_eq!(sc["id"], "uuid", "{b}");
    }
    // A write that declares nothing is folded under the declared analyzer.
    let (s, b) = write(
        &a,
        "docs",
        &json!({"durability": "durable", "documents": [
            {"id": UUID, "vector": [1.0, 0.0], "text": "he runs daily", "attributes": {"n": 7}},
        ]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&a).await;
    let (_, sum) = send(&a, "GET", "/v1/indexes/docs", &Value::Null).await;
    let (s, b) = query(&a, "docs", &json!({"text": "running", "top_k": 5})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["results"][0]["id"], UUID, "stemmed: {b} {sum}");
}

#[tokio::test]
async fn created_schema_refuses_other_declarations() {
    let store = Accounted::new(MemoryStore::new());
    let a = cached(&store, 1);
    let (s, b) = create(&a, "docs", &json!({"dims": 2, "schema": declared()})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let doc = json!({"id": UUID, "vector": [1.0, 0.0], "text": "x",
                     "attributes": {"n": 1, "sku": "a-1", "title": "le chat"}});
    let typed = json!({"id": UUID, "vector": [1.0, 0.0], "attributes": {"n": UUID}});
    for (what, schema, d) in [
        (
            "k1",
            json!({"text": {"type": "string", "full_text_search": {"k1": 1.0, "stemming": true}}}),
            doc.clone(),
        ),
        ("type", json!({"n": "uuid"}), typed),
        (
            "regex",
            json!({"code": {"type": "string", "regex": true}}),
            json!({"id": UUID, "vector": [1.0, 0.0], "attributes": {"code": "z"}}),
        ),
    ] {
        let (s, b) = write(
            &a,
            "docs",
            &json!({"durability": "durable", "documents": [d], "schema": schema}),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{what}: {b}");
        assert_eq!(b["error"]["code"], "schema_conflict", "{what}: {b}");
    }
    // The same map again is no conflict.
    let mut same = declared();
    same.as_object_mut().unwrap().remove("id");
    let (s, b) = write(
        &a,
        "docs",
        &json!({"durability": "durable", "documents": [doc], "schema": same}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn created_text_only_index() {
    let store = Accounted::new(MemoryStore::new());
    let a = cached(&store, 1);
    let schema = json!({"text": {"type": "string", "full_text_search": true}});
    let (s, b) = create(&a, "notes", &json!({"schema": schema})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(&a, "GET", "/v1/indexes/notes", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["schema"]["dims"], 0, "{b}");
    let (s, b) = query(&a, "notes", &json!({"text": "hello", "top_k": 5})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["results"], json!([]), "{b}");
    let (s, b) = query(&a, "notes", &json!({"vector": [1.0, 0.0], "top_k": 5})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "vector query: {b}");
    let (s, b) = write(
        &a,
        "notes",
        &json!({"durability": "durable", "documents": [{"id": "v", "vector": [1.0], "text": "x"}]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "vector write: {b}");
    let (s, b) = write(
        &a,
        "notes",
        &json!({"durability": "durable", "documents": [{"id": "t", "text": "hello world"}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "text write: {b}");
    fold(&a).await;
    let (s, b) = query(&a, "notes", &json!({"text": "hello", "top_k": 5})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["results"][0]["id"], "t", "{b}");
    // A metric with no width is refused; so is no width without a schema (M147).
    let (s, b) = create(
        &a,
        "m",
        &json!({"dims": 0, "distance_metric": "cosine_distance", "schema": schema}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, b) = create(&a, "m", &json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}

#[tokio::test]
async fn create_without_schema_is_m147s() {
    let with_none = Accounted::new(MemoryStore::new());
    let with_empty = Accounted::new(MemoryStore::new());
    let (a, b) = (cached(&with_none, 1), cached(&with_empty, 1));
    let (s, r) = create(
        &a,
        "docs",
        &json!({"dims": 3, "distance_metric": "cosine_distance"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, r) = create(
        &b,
        "docs",
        &json!({"dims": 3, "distance_metric": "cosine_distance", "schema": {}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (_, x) = send(&a, "GET", "/v1/indexes/docs", &Value::Null).await;
    let (_, y) = send(&b, "GET", "/v1/indexes/docs", &Value::Null).await;
    assert_eq!(x["schema"], y["schema"], "{x} vs {y}");
    assert_eq!(x["schema"]["regex"], json!([]), "{x}");
    assert_eq!(x["schema"]["attributes"], json!({}), "{x}");
    assert_eq!(x["schema"]["id"], "string", "{x}");
    // The whole schema, pinned: what M147's create recorded before M262. A schema-less
    // create recording any declaration, analyzer or further text field differs here.
    let golden = json!({"attributes": {}, "dims": 3, "distance_metric": "cosine_distance",
        "full_text_search": {"ascii_folding": false, "b": 0.75, "case_sensitive": false,
            "k1": 1.200_000_047_683_715_8, "language": "english", "remove_stopwords": false,
            "stemming": false, "tokenizer": "word_v1"},
        "id": "string", "regex": [], "text_field": ""});
    assert_eq!(x["schema"], golden, "{x}");
}

#[tokio::test]
async fn created_schema_refusals_match_writes() {
    let store = Accounted::new(MemoryStore::new());
    let a = cached(&store, 1);
    for (what, schema, says) in [
        (
            "unknown key",
            json!({"t": {"type": "string", "nope": 1}}),
            "a write declares",
        ),
        (
            "regex on id",
            json!({"id": {"type": "string", "regex": true}}),
            "cannot be declared regex",
        ),
        (
            "fts on uint64",
            json!({"n": {"type": "uint64", "full_text_search": true}}),
            "type is string or []string",
        ),
        (
            "stemming pre-tokenized",
            json!({"t": {"type": "[]string",
                         "full_text_search": {"tokenizer": "pre_tokenized_array", "stemming": true}}}),
            "pre_tokenized_array",
        ),
        (
            "named vector",
            json!({"emb": "[2]f16"}),
            "named vector field",
        ),
        ("vector width", json!({"vector": "[3]f32"}), "[2]f32"),
        // Anything but `[N]kind` on `vector` is refused, as a write refuses it.
        (
            "vector object",
            json!({"vector": {"type": "[2]f16"}}),
            "[2]f32",
        ),
        ("vector scalar", json!({"vector": "uint64"}), "[2]f32"),
        ("vector kind alone", json!({"vector": "f16"}), "[2]f32"),
        ("vector kind unknown", json!({"vector": "[2]f64"}), "[2]f32"),
    ] {
        let before = puts(&store);
        let (s, b) = create(&a, "docs", &json!({"dims": 2, "schema": schema})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{what}: {b}");
        assert!(b.to_string().contains(says), "{what}: {b}");
        assert_eq!(puts(&store), before, "{what} committed");
    }
    // A vector type of the create's width is recorded.
    let (s, b) = create(
        &a,
        "docs",
        &json!({"dims": 2, "schema": {"vector": "[2]f16"}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (_, b) = send(&a, "GET", "/v1/indexes/docs", &Value::Null).await;
    assert_eq!(b["schema"]["vector_types"]["vector"], "[2]f16", "{b}");
    // `[N]f32` is the default kind: accepted, and nothing further recorded.
    let (s, b) = create(
        &a,
        "plain",
        &json!({"dims": 2, "schema": {"vector": "[2]f32"}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (_, b) = send(&a, "GET", "/v1/indexes/plain", &Value::Null).await;
    assert!(b["schema"].get("vector_types").is_none(), "{b}");
    // An array type spelled as a string is not a named vector: the write's parser refuses it.
    let (s, b) = create(
        &a,
        "tags",
        &json!({"dims": 2, "schema": {"tags": "[]string"}}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(b.to_string().contains("a write declares"), "{b}");
    // A vector type on an index with no width says so.
    let (s, b) = create(
        &a,
        "nodims",
        &json!({"dims": 0, "schema": {"vector": "[2]f16",
            "text": {"type": "string", "full_text_search": true}}}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(b.to_string().contains("has no dims"), "{b}");
    // The width spelled as a write may spell it.
    let (s, b) = create(
        &a,
        "padded",
        &json!({"dims": 2, "schema": {"vector": "[02]i8"}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}
