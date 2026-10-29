//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Public sparse vectors (M178): a document's and a query's `sparse`, `{"key": weight}`, the
//! keys strings (M177) and the weights stored as f16.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_format::sparse::{ImpactEncoding, read_list, write_list};
use pstore_server::Api;
use pstore_testkit::depth::DepthCounting;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type A<S> = Arc<Api<S>>;

fn api() -> A<MemoryStore> {
    Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap()
}

async fn send_raw<S: pstore_blob::BlobStore + 'static>(
    api: &A<S>,
    method: &str,
    uri: &str,
    body: String,
) -> (StatusCode, Value) {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "178")
        .body(Body::from(body))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(r).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn send<S: pstore_blob::BlobStore + 'static>(
    api: &A<S>,
    method: &str,
    uri: &str,
    body: &Value,
) -> (StatusCode, Value) {
    send_raw(api, method, uri, body.to_string()).await
}

/// A durable write of `body` to `index`.
async fn put<S: pstore_blob::BlobStore + 'static>(
    api: &A<S>,
    index: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let mut body = body.clone();
    body["durability"] = json!("durable");
    send(api, "PUT", &format!("/v1/indexes/{index}/documents"), &body).await
}

async fn write<S: pstore_blob::BlobStore + 'static>(api: &A<S>, index: &str, body: Value) {
    let (s, b) = put(api, index, &body).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
}

async fn fold<S: pstore_blob::BlobStore + 'static>(api: &A<S>) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn ask<S: pstore_blob::BlobStore + 'static>(
    api: &A<S>,
    index: &str,
    q: &Value,
) -> (StatusCode, Value) {
    send(api, "POST", &format!("/v1/indexes/{index}/query"), q).await
}

async fn query<S: pstore_blob::BlobStore + 'static>(api: &A<S>, index: &str, q: &Value) -> Value {
    let (s, b) = ask(api, index, q).await;
    assert_eq!(s, StatusCode::OK, "{q}: {b}");
    b
}

/// `(id, score)` of each result.
fn hits(b: &Value) -> Vec<(String, f32)> {
    b["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results: {b}"))
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                r["score"].as_f64().unwrap() as f32,
            )
        })
        .collect()
}

fn ids(b: &Value) -> Vec<String> {
    hits(b).into_iter().map(|(id, _)| id).collect()
}

/// `w` as an f16 weight answers it, through the format's own encoding.
fn f16(w: f32) -> f32 {
    let mut raw = Vec::new();
    write_list(&mut raw, &[(0, w)], ImpactEncoding::F16, 0.0);
    read_list(&raw, 1, ImpactEncoding::F16, 0.0)[0].1
}

/// A deterministic stream.
fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

/// 300 rows of 5 keys from 2,000, weights in [-4, 4] to a thousandth with zeros among them.
fn corpus() -> Vec<(String, BTreeMap<String, f32>)> {
    let mut next = rng(0x9e37_79b9_7f4a_7c15);
    (0..300)
        .map(|i| {
            let mut m = BTreeMap::new();
            while m.len() < 5 {
                let k = format!("k{}", next() % 2_000);
                #[expect(clippy::cast_precision_loss, reason = "a fixture weight")]
                let w = match next() % 7 {
                    0 => 0.0,
                    _ => ((next() % 8_001) as f32 - 4_000.0) / 1_000.0,
                };
                m.insert(k, w);
            }
            (format!("r{i:03}"), m)
        })
        .collect()
}

/// The exhaustive answer: each row naming any query key, its dot product summed in f32 in key
/// byte order over the f16-rounded weights, best first by `total_cmp`, ties by id (the fold's
/// input order, the order written).
fn oracle(
    rows: &[(String, BTreeMap<String, f32>)],
    q: &BTreeMap<String, f32>,
    k: usize,
) -> Vec<(String, f32)> {
    let mut out: Vec<(String, f32)> = rows
        .iter()
        .filter(|(_, m)| q.keys().any(|key| m.contains_key(key)))
        .map(|(id, m)| {
            let s = q
                .iter()
                .filter_map(|(key, qw)| m.get(key).map(|w| qw * f16(*w)))
                .fold(0.0f32, |a, x| a + x);
            (id.clone(), s)
        })
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(k);
    out
}

fn queries() -> Vec<BTreeMap<String, f32>> {
    let rows = corpus();
    let mut next = rng(0x2545_f491_4f6c_dd1d);
    (0..50)
        .map(|n| {
            // Three keys some row names, and one no row does.
            let mut q = BTreeMap::new();
            while q.len() < 3 {
                let (_, m) = &rows[(next() % 300) as usize];
                let key = m.keys().nth((next() % 5) as usize).unwrap().clone();
                #[expect(clippy::cast_precision_loss, reason = "a fixture weight")]
                q.insert(key, ((next() % 2_001) as f32 - 1_000.0) / 500.0);
            }
            q.insert(format!("absent{n}"), 1.0);
            q
        })
        .collect()
}

#[tokio::test]
async fn a_sparse_query_ranks_as_an_exhaustive_dot_product_over_rows_and_columns() {
    // Criterion 1: one segment each, so ties are in write order; `sum` reports the raw dot
    // product. The two indexes are the same documents written as rows and as columns.
    let rows = corpus();
    let store = Accounted::new(MemoryStore::new());
    let a = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    write(
        &a,
        "rows",
        json!({"documents": rows.iter().map(|(id, m)| json!({"id": id, "sparse": m})).collect::<Vec<_>>()}),
    )
    .await;
    write(
        &a,
        "cols",
        json!({"upsert_columns": {
            "id": rows.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            "sparse": rows.iter().map(|(_, m)| m).collect::<Vec<_>>(),
        }}),
    )
    .await;
    fold(&a).await;
    let restarted = Api::unsecured(store, LaneId(2)).unwrap();
    for (n, q) in queries().iter().enumerate() {
        let want = oracle(&rows, q, 20);
        assert!(!want.is_empty());
        let body = json!({"sparse": q, "top_k": 20, "fusion": {"sum": {}}});
        let got = hits(&query(&restarted, "rows", &body).await);
        assert_eq!(got, want, "query {n}: {q:?}");
        assert_eq!(hits(&query(&restarted, "cols", &body).await), got, "{n}");
    }
}

/// An index of `n` rows with a dense vector, a text attribute and a sparse vector each: row
/// `i` names key `even` or `odd`, its text `alpha` or `beta`, and `n` is `i`.
async fn hybrid<S: pstore_blob::BlobStore + 'static>(a: &A<S>, n: usize) {
    let docs: Vec<Value> = (0..n)
        .map(|i| {
            #[expect(clippy::cast_precision_loss, reason = "a fixture vector")]
            let x = i as f32;
            json!({
                "id": format!("h{i:03}"),
                "vector": [1.0, x / 100.0],
                "text": if i % 3 == 0 { "alpha" } else { "beta" },
                "sparse": {if i % 2 == 0 { "even" } else { "odd" }: 1.0 + x / 1_000.0},
                "attributes": {"n": i},
            })
        })
        .collect();
    write(a, "h", json!({"documents": docs})).await;
    fold(a).await;
}

#[tokio::test]
async fn a_sparse_leg_fuses_last_and_is_filtered() {
    // Criterion 2.
    let a = api();
    hybrid(&a, 300).await;
    // The sparse leg is last: weighting it alone answers its ranking, the odd rows by
    // weight; weighting the other alone answers none of the sparse leg's order.
    let only = |w: [f32; 2], other: Value| {
        let mut q = json!({"sparse": {"odd": 1.0}, "top_k": 5,
            "fusion": {"rrf": {"weights": w}}});
        for (k, v) in other.as_object().unwrap() {
            q[k] = v.clone();
        }
        q
    };
    for other in [json!({"vector": [1.0, 0.0]}), json!({"text": "alpha"})] {
        let sparse_only = ids(&query(&a, "h", &only([0.0, 1.0], other.clone())).await);
        assert_eq!(
            sparse_only,
            ["h299", "h297", "h295", "h293", "h291"],
            "{other}"
        );
        let other_only = ids(&query(&a, "h", &only([1.0, 0.0], other.clone())).await);
        assert_ne!(other_only, sparse_only, "{other}");
        // Both legs: each leg's first row, which no other leg ranks first.
        let both = ids(&query(&a, "h", &only([1.0, 1.0], other.clone())).await);
        assert_ne!(other_only[0], sparse_only[0], "{other}");
        assert!(
            both.contains(&sparse_only[0]) && both.contains(&other_only[0]),
            "{other}: {both:?}"
        );
    }
    // A filter admitting 40 rows, 20 of them odd: a full top 20 of admitted odd rows.
    let b = query(
        &a,
        "h",
        &json!({"sparse": {"odd": 1.0}, "top_k": 20, "filters": ["n", "Lt", 40]}),
    )
    .await;
    let got = ids(&b);
    let want: Vec<String> = (0..40)
        .rev()
        .filter(|i| i % 2 == 1)
        .map(|i| format!("h{i:03}"))
        .collect();
    assert_eq!(got, want);
}

#[tokio::test]
async fn what_no_sparse_vector_spells_is_refused_naming_the_rule() {
    // Criterion 3: each refused, naming its document or query and its bound; nothing buffered.
    let a = api();
    let long = "k".repeat(257);
    let many: BTreeMap<String, f32> = (0..1_025).map(|i| (format!("k{i}"), 1.0)).collect();
    let cases: Vec<(Value, &str)> = vec![
        (json!({}), "0 keys: a sparse vector holds 1 to 1024"),
        (
            json!({long.clone(): 1.0}),
            "a key of 257 bytes: a key is 1 to 256",
        ),
        (json!({"": 1.0}), "a key of 0 bytes: a key is 1 to 256"),
        (json!({"a": "1"}), "weight \"1\" is not a number"),
        (json!({"a": null}), "weight null is not a number"),
        (
            json!({"a": 65_505.0}),
            "is not a number of magnitude at most 65504",
        ),
        (
            json!({"a": 1e39}),
            "is not a number of magnitude at most 65504",
        ),
        (json!(["a", 1.0]), "an object of key to weight"),
    ];
    for (sparse, why) in &cases {
        let (s, b) = put(
            &a,
            "bad",
            &json!({"documents": [{"id": "d1", "sparse": sparse}]}),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{sparse}: {b}");
        let msg = b["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("document d1: sparse:") && msg.contains(why),
            "{sparse}: {b}"
        );
        let (s, b) = ask(&a, "bad", &json!({"sparse": sparse, "top_k": 1})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "query {sparse}: {b}");
        let msg = b["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.starts_with("sparse: ") && msg.contains(why),
            "query {sparse}: {b}"
        );
    }
    // 1,025 keys pass the body's own bound on an object's members (`shape`), refused first.
    for body in [
        json!({"documents": [{"id": "d1", "sparse": many}]}),
        json!({"sparse": many, "top_k": 1}),
    ] {
        let uri = if body.get("documents").is_some() {
            "/v1/indexes/bad/documents"
        } else {
            "/v1/indexes/bad/query"
        };
        let method = if body.get("documents").is_some() {
            "PUT"
        } else {
            "POST"
        };
        let (s, b) = send(&a, method, uri, &body).await;
        assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{b}");
        assert!(b.to_string().contains("more than 1024 members"), "{b}");
    }
    // The named field and the patch paths.
    let (s, b) = put(
        &a,
        "bad",
        &json!({"documents": [{"id": "d1", "vector": [1.0], "vectors": {"sparse": [[1.0]]}}]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(
        b.to_string().contains("`sparse` is the sparse field"),
        "{b}"
    );
    let (s, b) = put(
        &a,
        "bad",
        &json!({"patch_rows": [{"id": "d1", "sparse": {"a": 1.0}}]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(b.to_string().contains("a patch carries no sparse"), "{b}");
    let (s, b) = put(
        &a,
        "bad",
        &json!({"patch_columns": {"id": ["d1"], "sparse": [{"a": 1.0}]}}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(b.to_string().contains("a patch carries no sparse"), "{b}");
    // Nothing of any of them was buffered: a query naming a key from each refused body that
    // a query can spell answers only the row written after them.
    write(
        &a,
        "bad",
        json!({"documents": [{"id": "ok", "sparse": {"z": 1.0}}]}),
    )
    .await;
    let every = json!({"sparse": {"a": 1.0, "z": 1.0, "k0": 1.0}, "top_k": 10});
    assert_eq!(ids(&query(&a, "bad", &every).await), ["ok"]);
    // Each bound itself is admitted, in a document and in a query.
    let edge = json!({"k".repeat(256): 65_504.0, "neg": -65_504.0});
    write(
        &a,
        "bad",
        json!({"documents": [{"id": "edge", "sparse": edge}]}),
    )
    .await;
    let b = query(&a, "bad", &json!({"sparse": edge, "top_k": 1})).await;
    assert_eq!(ids(&b), ["edge"]);
    let most: BTreeMap<String, f32> = (0..1_024).map(|i| (format!("m{i}"), 1.0)).collect();
    write(
        &a,
        "bad",
        json!({"documents": [{"id": "most", "sparse": most}]}),
    )
    .await;
    let b = query(&a, "bad", &json!({"sparse": most, "top_k": 1})).await;
    assert_eq!(ids(&b), ["most"]);
}

#[tokio::test]
async fn a_sparse_leg_takes_no_order_aggregation_or_sum_beside_text() {
    // Criterion 3's combinations: an order and an aggregation are not rankings, and a sum
    // or max beside text legs would add a score that can be negative.
    let a = api();
    hybrid(&a, 10).await;
    let leg = json!({"odd": 1.0});
    for (q, why) in [
        (
            json!({"sparse": leg, "rank_by": ["n", "asc"], "top_k": 3}),
            "rank_by orders by an attribute; it cannot be combined with vector, multi, text, \
             sparse",
        ),
        (
            json!({"sparse": leg, "aggregate_by": {"c": ["Count", "id"]}}),
            "aggregate_by cannot be combined with sparse",
        ),
        (
            json!({"sparse": leg, "text": "alpha", "top_k": 3, "fusion": {"sum": {}}}),
            "a sparse leg's can be negative, so it takes sum only alone",
        ),
        (
            json!({"sparse": leg, "text": "alpha", "top_k": 3, "fusion": {"max": {}}}),
            "a sparse leg's can be negative, so it takes max only alone",
        ),
    ] {
        let (s, b) = ask(&a, "h", &q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{q}: {b}");
        let msg = b["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains(why), "{q}: {b}");
    }
    // Alone, a sum is the leg's own score: the dot product, negative weights included.
    let b = query(
        &a,
        "h",
        &json!({"sparse": {"odd": -2.0}, "top_k": 1, "fusion": {"sum": {}}}),
    )
    .await;
    assert_eq!(hits(&b), [("h001".to_owned(), -2.0 * f16(1.001))]);
    // A sparse leg is a ranking, so facets take it: they count the filtered set beside it.
    let (s, b) = ask(
        &a,
        "h",
        &json!({"sparse": leg, "top_k": 3, "facets": {"aggregate_by": {"c": ["Count", "id"]}}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ids(&b), ["h009", "h007", "h005"]);
    assert_eq!(b["facets"]["c"], 10, "{b}");
}

#[tokio::test]
async fn a_key_twice_is_its_last_weight_and_a_patch_keeps_the_field() {
    // Criterion 3's last clauses.
    let a = api();
    let (s, b) = send_raw(
        &a,
        "PUT",
        "/v1/indexes/t/documents",
        r#"{"durability": "durable", "documents": [{"id": "d", "sparse": {"a": 1.0, "a": 3.0}}]}"#
            .to_owned(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let score = |b: Value| hits(&b)[0].1;
    let q = json!({"sparse": {"a": 1.0}, "top_k": 1, "fusion": {"sum": {}}});
    assert_eq!(score(query(&a, "t", &q).await), 3.0);
    write(
        &a,
        "t",
        json!({"patch_rows": [{"id": "d", "attributes": {"n": 1}}]}),
    )
    .await;
    fold(&a).await;
    assert_eq!(
        score(query(&a, "t", &q).await),
        3.0,
        "the patch dropped the field"
    );
}

#[tokio::test]
async fn a_sparse_query_answers_unfolded_rows_nothing_for_absent_keys_and_no_dense_leg() {
    // Criterion 4.
    let a = api();
    write(
        &a,
        "s",
        json!({"documents": [{"id": "x", "sparse": {"a": 1.0}}]}),
    )
    .await;
    let q = json!({"sparse": {"a": 1.0}, "top_k": 5});
    assert_eq!(ids(&query(&a, "s", &q).await), ["x"], "the unfolded row");
    fold(&a).await;
    let none = query(&a, "s", &json!({"sparse": {"nobody": 1.0}, "top_k": 5})).await;
    assert_eq!(ids(&none), Vec::<String>::new());
    let (s, b) = ask(&a, "s", &json!({"vector": [1.0], "top_k": 5})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(b.to_string().contains("schema_conflict"), "{b}");
}

#[tokio::test]
async fn a_sparse_query_costs_what_a_dense_one_does() {
    // Criterion 5: D-34 as M47 restates it, three rounds to rank and one for the rows, the
    // dictionary fetched beside the footer rather than after it.
    let store = DepthCounting::new(MemoryStore::new());
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    let docs: Vec<Value> = (0..50)
        .map(
            |i| json!({"id": format!("d{i}"), "sparse": {format!("k{}", i % 10): 1.0, "all": 0.5}}),
        )
        .collect();
    write(&a, "dep", json!({"documents": docs})).await;
    fold(&a).await;
    store.reset();
    let b = query(
        &a,
        "dep",
        &json!({"sparse": {"k3": 1.0, "all": 1.0}, "top_k": 10}),
    )
    .await;
    assert_eq!(ids(&b).len(), 10);
    assert_eq!(
        store.depth(),
        4,
        "a sparse query took {} rounds",
        store.depth()
    );
    assert_eq!(
        store.requests(),
        REQUESTS,
        "a sparse query issued {} requests",
        store.requests()
    );
}

/// Measured on the first run and pinned: HEAD, the segment's footer with its dictionary
/// beside it, the postings, the rows.
const REQUESTS: usize = 5;
