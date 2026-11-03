//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Stored vectors in results and exports (M179): `include_vectors: ["vector"]` on a ranked
//! query or a `rank_by` order, spelled `float` or `base64`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
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

async fn send<S: pstore_blob::BlobStore + 'static>(
    api: &A<S>,
    method: &str,
    uri: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "179")
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

async fn write<S: pstore_blob::BlobStore + 'static>(api: &A<S>, index: &str, body: Value) {
    let mut body = body;
    body["durability"] = json!("durable");
    let (s, b) = send(api, "PUT", &format!("/v1/indexes/{index}/documents"), &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
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

fn ids(b: &Value) -> Vec<String> {
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

/// A result's `vector`, from either spelling, as f32 bits.
fn vector_of(r: &Value) -> Option<Vec<u32>> {
    match &r["vector"] {
        Value::Null => None,
        Value::Array(a) => Some(
            a.iter()
                .map(|x| {
                    #[expect(clippy::cast_possible_truncation, reason = "an f32's spelling")]
                    let f = x.as_f64().unwrap() as f32;
                    f.to_bits()
                })
                .collect(),
        ),
        Value::String(s) => {
            let bytes = base64::engine::general_purpose::STANDARD.decode(s).unwrap();
            Some(
                bytes
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            )
        }
        other => panic!("a vector is a list or a string: {other}"),
    }
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

/// A deterministic stream.
fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed | 1;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

/// `n` vectors of width 8, components in [-1, 1) with many bits set, keyed by id.
fn corpus(n: usize, seed: u64) -> BTreeMap<String, Vec<f32>> {
    let mut next = rng(seed);
    (0..n)
        .map(|i| {
            let v: Vec<f32> = (0..8)
                .map(|_| {
                    #[expect(clippy::cast_precision_loss, reason = "a fixture")]
                    let x = (next() >> 40) as f32 / (1u64 << 23) as f32;
                    x - 1.0
                })
                .collect();
            (format!("r{i:04}"), v)
        })
        .collect()
}

fn docs(rows: &BTreeMap<String, Vec<f32>>) -> Vec<Value> {
    rows.iter()
        .map(|(id, v)| json!({"id": id, "vector": v, "attributes": {"n": id}}))
        .collect()
}

const METRICS: [&str; 3] = ["dot_product", "euclidean_squared", "cosine_distance"];

/// 300 rows folded and 50 more unfolded, under `metric`, read back by an API restarted over
/// the same store.
async fn built(metric: &str) -> (A<MemoryStore>, BTreeMap<String, Vec<f32>>) {
    let store = Accounted::new(MemoryStore::new());
    let a = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    let all = corpus(350, 179);
    let (folded, fresh): (BTreeMap<_, _>, BTreeMap<_, _>) = all
        .clone()
        .into_iter()
        .partition(|(id, _)| id.as_str() < "r0300");
    write(
        &a,
        "v",
        json!({"distance_metric": metric, "documents": docs(&folded)}),
    )
    .await;
    fold(&a).await;
    let b = Api::unsecured(store, LaneId(2)).unwrap();
    write(
        &b,
        "v",
        json!({"distance_metric": metric, "documents": docs(&fresh)}),
    )
    .await;
    (b, all)
}

/// The written vector as `metric` stores it, client side: cosine's normalized.
fn expected(metric: &str, v: &[f32]) -> Vec<f32> {
    if metric == "cosine_distance" {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    } else {
        v.to_vec()
    }
}

fn assert_vector(metric: &str, got: &[u32], written: &[f32], id: &str) {
    let want = expected(metric, written);
    if metric == "cosine_distance" {
        assert_eq!(got.len(), want.len(), "{id}");
        for (g, w) in got.iter().zip(&want) {
            assert!(
                (f32::from_bits(*g) - w).abs() <= 1e-6,
                "{id}: {got:?} vs {want:?}"
            );
        }
    } else {
        assert_eq!(got, bits(&want), "{metric} {id}");
    }
}

#[tokio::test]
async fn a_ranked_query_answers_each_rows_stored_vector_in_either_spelling() {
    // Criterion 1.
    for metric in METRICS {
        let (a, rows) = built(metric).await;
        let mut next = rng(7);
        for n in 0..10 {
            #[expect(clippy::cast_precision_loss, reason = "a fixture")]
            let q: Vec<f32> = (0..8).map(|_| (next() % 100) as f32 / 50.0 - 1.0).collect();
            let float = query(
                &a,
                "v",
                &json!({"vector": q, "top_k": 40, "exact": true, "include_vectors": ["vector"]}),
            )
            .await;
            let b64 = query(
                &a,
                "v",
                &json!({"vector": q, "top_k": 40, "exact": true, "include_vectors": ["vector"],
                    "vector_encoding": "base64"}),
            )
            .await;
            let results = float["results"].as_array().unwrap();
            assert_eq!(results.len(), 40, "{metric} {n}");
            assert!(results.iter().any(|r| r["id"].as_str().unwrap() >= "r0300"));
            for (f, s) in results.iter().zip(b64["results"].as_array().unwrap()) {
                let id = f["id"].as_str().unwrap();
                assert!(s["vector"].is_string(), "{s}");
                let got = vector_of(f).unwrap();
                assert_eq!(
                    vector_of(s).unwrap(),
                    got,
                    "{metric} {id}: base64 and float"
                );
                assert_vector(metric, &got, &rows[id], id);
            }
        }
        // Without `include_vectors`, no row carries one.
        let plain = query(&a, "v", &json!({"vector": vec![1.0_f32; 8], "top_k": 5})).await;
        assert!(plain["results"][0].get("vector").is_none(), "{plain}");
    }
}

#[tokio::test]
async fn an_export_by_id_pages_rebuild_the_index_exactly() {
    // Criterion 2: 64 rows a page, by `offset`, written into a new index of the same metric.
    for metric in METRICS {
        let (a, rows) = built(metric).await;
        let mut pages = Vec::new();
        for page in 0.. {
            let b = query(
                &a,
                "v",
                &json!({"rank_by": ["id", "asc"], "top_k": 64, "offset": page * 64,
                    "include_vectors": ["vector"], "include_attributes": true,
                    "vector_encoding": if page % 2 == 0 { "float" } else { "base64" }}),
            )
            .await;
            let got = b["results"].as_array().unwrap().clone();
            if got.is_empty() {
                break;
            }
            pages.extend(got);
        }
        assert_eq!(pages.len(), rows.len(), "{metric}");
        let mut copy = Vec::new();
        for r in &pages {
            let id = r["id"].as_str().unwrap();
            let got = vector_of(r).unwrap_or_else(|| panic!("{metric} {id}: {r}"));
            assert_vector(metric, &got, &rows[id], id);
            let v: Vec<f32> = got.iter().map(|b| f32::from_bits(*b)).collect();
            copy.push(json!({"id": id, "vector": v, "attributes": r["attributes"]}));
        }
        write(
            &a,
            "copy",
            json!({"distance_metric": metric, "documents": copy}),
        )
        .await;
        let mut next = rng(11);
        for n in 0..20 {
            #[expect(clippy::cast_precision_loss, reason = "a fixture")]
            let q: Vec<f32> = (0..8).map(|_| (next() % 100) as f32 / 50.0 - 1.0).collect();
            let body = json!({"vector": q, "top_k": 15, "exact": true, "include_attributes": true});
            let (x, y) = (query(&a, "v", &body).await, query(&a, "copy", &body).await);
            assert_eq!(ids(&x), ids(&y), "{metric} {n}");
            for (r, s) in x["results"]
                .as_array()
                .unwrap()
                .iter()
                .zip(y["results"].as_array().unwrap())
            {
                let (d, e) = (r["$dist"].as_f64().unwrap(), s["$dist"].as_f64().unwrap());
                if metric == "cosine_distance" {
                    assert!((d - e).abs() <= 1e-5, "{metric} {n}: {d} vs {e}");
                } else {
                    assert_eq!(d, e, "{metric} {n}");
                }
                assert_eq!(r["attributes"], s["attributes"], "{metric} {n}");
            }
        }
    }
}

#[tokio::test]
async fn an_index_never_folded_exports_in_its_own_metric() {
    // Criterion 2's unfolded rows, with no schema yet: the metric is the rows' own, so the
    // euclidean transform's extra component is removed from them too.
    let a = api();
    let rows = corpus(10, 5);
    write(
        &a,
        "e",
        json!({"distance_metric": "euclidean_squared", "documents": docs(&rows)}),
    )
    .await;
    let b = query(
        &a,
        "e",
        &json!({"rank_by": ["id", "asc"], "top_k": 10, "include_vectors": ["vector"]}),
    )
    .await;
    assert_eq!(ids(&b).len(), 10, "{b}");
    for r in b["results"].as_array().unwrap() {
        let id = r["id"].as_str().unwrap();
        assert_eq!(vector_of(r).unwrap(), bits(&rows[id]), "{id}");
    }
}

#[tokio::test]
async fn a_row_without_a_vector_answers_none_and_diversify_keeps_its_picks() {
    // Criterion 3's rows.
    let a = api();
    // A width-0 index (M170), one row folded and one unfolded; and a sparse-only index.
    write(
        &a,
        "t",
        json!({"documents": [{"id": "f", "text": "alpha"}]}),
    )
    .await;
    write(
        &a,
        "s",
        json!({"documents": [{"id": "s", "sparse": {"k": 1.0}}]}),
    )
    .await;
    fold(&a).await;
    write(
        &a,
        "t",
        json!({"documents": [{"id": "u", "text": "alpha"}]}),
    )
    .await;
    let with = |q: Value| {
        let mut q = q;
        q["include_vectors"] = json!(["vector"]);
        q
    };
    for (index, q, want) in [
        ("t", json!({"text": "alpha", "top_k": 5}), 2),
        ("t", json!({"rank_by": ["id", "asc"], "top_k": 5}), 2),
        ("s", json!({"sparse": {"k": 1.0}, "top_k": 5}), 1),
        ("s", json!({"rank_by": ["id", "asc"], "top_k": 5}), 1),
    ] {
        let b = query(&a, index, &with(q.clone())).await;
        let rows = b["results"].as_array().unwrap();
        assert_eq!(rows.len(), want, "{q}: {b}");
        for r in rows {
            assert!(r.get("vector").is_none(), "{q}: {r}");
        }
    }
    // With `diversify`, the rows it picks answer their vectors.
    let rows = corpus(40, 3);
    write(&a, "d", json!({"documents": docs(&rows)})).await;
    fold(&a).await;
    let b = query(
        &a,
        "d",
        &with(json!({"vector": vec![1.0_f32; 8], "top_k": 5, "diversify": {"lambda": 0.5, "window": 20}})),
    )
    .await;
    let picked = b["results"].as_array().unwrap();
    assert_eq!(picked.len(), 5, "{b}");
    for r in picked {
        let id = r["id"].as_str().unwrap();
        assert_eq!(vector_of(r).unwrap(), bits(&rows[id]), "{id}");
    }
}

#[tokio::test]
async fn what_cannot_be_projected_is_refused_naming_the_rule() {
    // Criterion 3's refusals.
    let a = api();
    write(&a, "r", json!({"documents": docs(&corpus(5, 1))})).await;
    let ranked = json!({"vector": vec![1.0_f32; 8], "top_k": 3});
    let ordered = json!({"rank_by": ["id", "asc"], "top_k": 3});
    let plus = |q: &Value, extra: Value| {
        let mut q = q.clone();
        for (k, v) in extra.as_object().unwrap() {
            q[k] = v.clone();
        }
        q
    };
    let v = json!(["vector"]);
    let cases = [
        (
            // M190: a named field is projected; `sparse` is not yet.
            plus(&ranked, json!({"include_vectors": ["sparse"]})),
            "include_vectors: `sparse` is not projected",
        ),
        (
            plus(&ranked, json!({"include_vectors": []})),
            "include_vectors: a list of the vector fields",
        ),
        (
            plus(&ranked, json!({"include_vectors": "vector"})),
            "include_vectors: a list of the vector fields",
        ),
        (
            plus(&ordered, json!({"include_vectors": true})),
            "include_vectors: a list of the vector fields",
        ),
        (
            plus(
                &ranked,
                json!({"include_vectors": v, "vector_encoding": "f16"}),
            ),
            "vector_encoding: \"f16\" is not float or base64",
        ),
        (
            plus(&ranked, json!({"vector_encoding": "float"})),
            "vector_encoding spells projected vectors",
        ),
        (
            json!({"aggregate_by": {"c": ["Count", "id"]}, "include_vectors": v}),
            "aggregate_by cannot be combined with include_vectors",
        ),
        (
            plus(&ranked, json!({"include_vectors": v, "as_of": 1})),
            "cannot be combined with as_of",
        ),
        (
            plus(&ranked, json!({"include_vectors": v, "top_k": 1_001})),
            "1001 rows: vectors are projected for at most 1000",
        ),
        (
            plus(
                &ranked,
                json!({"include_vectors": v, "top_k": 1_000, "offset": 1}),
            ),
            "1001 rows: vectors are projected for at most 1000",
        ),
        (
            plus(&ordered, json!({"include_vectors": v, "top_k": 1_001})),
            "1001 rows: vectors are projected for at most 1000",
        ),
        (
            json!({"queries": [plus(&ranked, json!({"include_vectors": v}))]}),
            "a query inside queries cannot project vectors",
        ),
    ];
    for (q, why) in &cases {
        let (s, b) = ask(&a, "r", q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{q}: {b}");
        let msg = b["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains(why), "{q}: {b}");
    }
    // Each bound itself is admitted.
    for q in [
        plus(&ranked, json!({"include_vectors": v, "top_k": 1_000})),
        plus(
            &ordered,
            json!({"include_vectors": v, "top_k": 1_000, "offset": 1_000}),
        ),
    ] {
        let (s, b) = ask(&a, "r", &q).await;
        assert_eq!(s, StatusCode::OK, "{q}: {b}");
    }
}

/// One cold query's `(depth, requests)` over an index of `segments` folded segments.
async fn cost(segments: usize, q: &Value) -> (usize, usize) {
    let store = DepthCounting::new(MemoryStore::new());
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    for s in 0..segments {
        let rows = corpus(20, 100 + s as u64);
        let rows: BTreeMap<String, Vec<f32>> = rows
            .into_iter()
            .map(|(id, v)| (format!("{id}-{s}"), v))
            .collect();
        write(&a, "c", json!({"documents": docs(&rows)})).await;
        fold(&a).await;
    }
    let cold = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
    store.reset();
    let b = query(&cold, "c", q).await;
    assert_eq!(ids(&b).len(), 10, "{q}: {b}");
    (store.depth(), store.requests())
}

#[tokio::test]
async fn a_projection_costs_no_round_on_a_ranking_and_one_on_an_order() {
    // Criterion 4, at 1 and at 4 segments.
    let ranked = json!({"vector": vec![1.0_f32; 8], "top_k": 10, "exact": true});
    let ordered = json!({"rank_by": ["id", "asc"], "top_k": 10});
    let with = |q: &Value| {
        let mut q = q.clone();
        q["include_vectors"] = json!(["vector"]);
        q
    };
    for (segments, pinned) in [(1, PINNED_1), (4, PINNED_4)] {
        let r = cost(segments, &ranked).await;
        let rv = cost(segments, &with(&ranked)).await;
        let o = cost(segments, &ordered).await;
        let ov = cost(segments, &with(&ordered)).await;
        assert_eq!(rv.0, r.0, "{segments}: a ranked projection added a round");
        assert!(rv.1 <= r.1 + segments, "{segments}: {rv:?} vs {r:?}");
        assert_eq!(
            ov.0, 4,
            "{segments}: an order's projection is one round more: {ov:?}"
        );
        assert_eq!(o.0, 3, "{segments}: an order alone: {o:?}");
        assert_eq!([r, rv, o, ov], pinned, "{segments} segments");
    }
}

/// `(depth, requests)` of a ranked query, it with vectors, an order, and it with vectors, at
/// one segment, measured on the first run and pinned. Without vectors, the parent's.
const PINNED_1: [(usize, usize); 4] = [(4, 4), (4, 5), (3, 3), (4, 4)];
/// The same at four segments.
const PINNED_4: [(usize, usize); 4] = [(4, 13), (4, 17), (3, 9), (4, 13)];
