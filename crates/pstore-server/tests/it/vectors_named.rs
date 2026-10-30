//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Named and multi-vector fields in results and exports (M190): `include_vectors` naming a
//! document's named fields, answered under `vectors` as a write spells them.

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
        .header("x-pstore-tenant", "190")
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

/// One spelled vector, from either spelling, as f32 bits.
fn decoded(v: &Value) -> Vec<u32> {
    match v {
        Value::Array(a) => a
            .iter()
            .map(|x| {
                #[expect(clippy::cast_possible_truncation, reason = "an f32's spelling")]
                let f = x.as_f64().unwrap() as f32;
                f.to_bits()
            })
            .collect(),
        Value::String(s) => base64::engine::general_purpose::STANDARD
            .decode(s)
            .unwrap()
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        other => panic!("a vector is a list or a string: {other}"),
    }
}

/// A result's named field `name`, each vector as f32 bits; `None` when it has no entry.
fn named_of(r: &Value, name: &str) -> Option<Vec<Vec<u32>>> {
    let vs = r.get("vectors")?.get(name)?;
    Some(vs.as_array().unwrap().iter().map(decoded).collect())
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

fn floats(v: &[u32]) -> Vec<f32> {
    v.iter().map(|b| f32::from_bits(*b)).collect()
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

/// A vector of `width` components in [-1, 1) with many bits set.
fn vector(next: &mut impl FnMut() -> u64, width: usize) -> Vec<f32> {
    (0..width)
        .map(|_| {
            #[expect(clippy::cast_precision_loss, reason = "a fixture")]
            let x = (next() >> 40) as f32 / (1u64 << 23) as f32;
            x - 1.0
        })
        .collect()
}

/// A row as written: `vector`, and its named fields.
#[derive(Clone)]
struct Row {
    vector: Vec<f32>,
    named: BTreeMap<String, Vec<Vec<f32>>>,
}

/// Rows `from..to`, each with `vector` (width 8) and `title` (one vector of width 4), and
/// with `tokens` (width 4) of `tokens(i)` vectors where that is not 0.
fn rows(
    from: usize,
    to: usize,
    seed: u64,
    tokens: impl Fn(usize) -> usize,
) -> BTreeMap<String, Row> {
    let mut next = rng(seed);
    (from..to)
        .map(|i| {
            let mut named = BTreeMap::new();
            named.insert("title".to_owned(), vec![vector(&mut next, 4)]);
            let n = tokens(i);
            if n > 0 {
                named.insert(
                    "tokens".to_owned(),
                    (0..n).map(|_| vector(&mut next, 4)).collect(),
                );
            }
            (
                format!("r{i:04}"),
                Row {
                    vector: vector(&mut next, 8),
                    named,
                },
            )
        })
        .collect()
}

fn docs(rows: &BTreeMap<String, Row>) -> Vec<Value> {
    rows.iter()
        .map(|(id, r)| json!({"id": id, "vector": r.vector, "vectors": r.named, "attributes": {"n": id}}))
        .collect()
}

const METRICS: [&str; 3] = ["dot_product", "euclidean_squared", "cosine_distance"];

const ALL: [&str; 3] = ["vector", "title", "tokens"];

/// 300 rows folded in two segments, each holding one `title`, and 50 more unfolded that
/// also hold 1 to 5 `tokens` -- a folded multi-vector field is M190.2's -- under `metric`,
/// read back by an API restarted over the same store.
async fn built(metric: &str) -> (A<MemoryStore>, BTreeMap<String, Row>) {
    let store = Accounted::new(MemoryStore::new());
    let a = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    let first = rows(0, 150, 190, |_| 0);
    let second = rows(150, 300, 191, |_| 0);
    let fresh = rows(300, 350, 192, |i| 1 + i % 5);
    for part in [&first, &second] {
        write(
            &a,
            "v",
            json!({"distance_metric": metric, "documents": docs(part)}),
        )
        .await;
        fold(&a).await;
    }
    let b = Api::unsecured(store, LaneId(2)).unwrap();
    write(
        &b,
        "v",
        json!({"distance_metric": metric, "documents": docs(&fresh)}),
    )
    .await;
    let mut all = first;
    all.extend(second);
    all.extend(fresh);
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

fn assert_vector(metric: &str, got: &[u32], written: &[f32], what: &str) {
    let want = expected(metric, written);
    if metric == "cosine_distance" {
        assert_eq!(got.len(), want.len(), "{what}");
        for (g, w) in got.iter().zip(&want) {
            assert!(
                (f32::from_bits(*g) - w).abs() <= 1e-6,
                "{what}: {got:?} vs {want:?}"
            );
        }
    } else {
        assert_eq!(got, bits(&want), "{metric} {what}");
    }
}

/// Every named field of `r`, as projected, against the row written: the same fields, each
/// vector in order.
fn assert_named(metric: &str, r: &Value, written: &Row) {
    let id = r["id"].as_str().unwrap();
    let keys: Vec<&str> = r["vectors"]
        .as_object()
        .unwrap_or_else(|| panic!("{id}: {r}"))
        .keys()
        .map(String::as_str)
        .collect();
    let want: Vec<&str> = written.named.keys().map(String::as_str).collect();
    assert_eq!(keys, want, "{metric} {id}");
    for (name, vs) in &written.named {
        let got = named_of(r, name).unwrap();
        assert_eq!(got.len(), vs.len(), "{metric} {id} {name}");
        for (k, (g, w)) in got.iter().zip(vs).enumerate() {
            assert_vector(metric, g, w, &format!("{id} {name}[{k}]"));
        }
    }
}

#[tokio::test]
async fn a_ranked_query_answers_each_rows_named_fields_in_either_spelling() {
    // Criterion 1.
    for metric in METRICS {
        let (a, rows) = built(metric).await;
        let mut next = rng(7);
        for n in 0..10 {
            let q = vector(&mut next, 8);
            let float = query(
                &a,
                "v",
                &json!({"vector": q, "top_k": 60, "exact": true, "include_vectors": ALL}),
            )
            .await;
            let b64 = query(
                &a,
                "v",
                &json!({"vector": q, "top_k": 60, "exact": true, "include_vectors": ALL,
                    "vector_encoding": "base64"}),
            )
            .await;
            let results = float["results"].as_array().unwrap();
            assert_eq!(results.len(), 60, "{metric} {n}");
            assert!(results.iter().any(|r| r["id"].as_str().unwrap() >= "r0300"));
            assert!(results.iter().any(|r| r["id"].as_str().unwrap() < "r0150"));
            for (f, s) in results.iter().zip(b64["results"].as_array().unwrap()) {
                let id = f["id"].as_str().unwrap();
                assert!(s["vectors"]["title"][0].is_string(), "{s}");
                for name in ["title", "tokens"] {
                    assert_eq!(named_of(f, name), named_of(s, name), "{metric} {id} {name}");
                }
                assert_named(metric, f, &rows[id]);
                assert_vector(metric, &decoded(&f["vector"]), &rows[id].vector, id);
            }
        }
        // Without a named field asked, no row carries `vectors`.
        let plain = query(
            &a,
            "v",
            &json!({"vector": vec![1.0_f32; 8], "top_k": 5, "include_vectors": ["vector"]}),
        )
        .await;
        assert!(plain["results"][0].get("vectors").is_none(), "{plain}");
    }
}

#[tokio::test]
async fn an_export_by_id_pages_rebuild_every_field_exactly() {
    // Criterion 2: 64 rows a page, by `offset`, written into a new index of the same metric.
    for metric in METRICS {
        let (a, rows) = built(metric).await;
        let mut pages = Vec::new();
        for page in 0.. {
            let b = query(
                &a,
                "v",
                &json!({"rank_by": ["id", "asc"], "top_k": 64, "offset": page * 64,
                    "include_vectors": ALL, "include_attributes": true,
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
            assert_named(metric, r, &rows[id]);
            let named: BTreeMap<String, Vec<Vec<f32>>> = r["vectors"]
                .as_object()
                .unwrap()
                .keys()
                .map(|name| {
                    let vs = named_of(r, name).unwrap();
                    (name.clone(), vs.iter().map(|v| floats(v)).collect())
                })
                .collect();
            copy.push(json!({"id": id, "vector": floats(&decoded(&r["vector"])),
                "vectors": named, "attributes": r["attributes"]}));
        }
        write(
            &a,
            "copy",
            json!({"distance_metric": metric, "documents": copy}),
        )
        .await;
        let mut next = rng(11);
        for n in 0..60 {
            let body = match n % 3 {
                0 => json!({"vector": vector(&mut next, 8), "exact": true}),
                1 => json!({"vector": vector(&mut next, 4), "field": "title"}),
                _ => json!({"multi": {"field": "tokens",
                    "vectors": [vector(&mut next, 4), vector(&mut next, 4)]}}),
            };
            let mut body = body;
            body["top_k"] = json!(15);
            body["include_attributes"] = json!(true);
            let (x, y) = (query(&a, "v", &body).await, query(&a, "copy", &body).await);
            assert_eq!(ids(&x), ids(&y), "{metric} {n}: {body}");
            assert_eq!(ids(&x).len(), 15, "{metric} {n}");
            for (r, s) in x["results"]
                .as_array()
                .unwrap()
                .iter()
                .zip(y["results"].as_array().unwrap())
            {
                // A named or `multi` leg answers its score, `vector`'s its `$dist`.
                let key = if n % 3 == 0 { "$dist" } else { "score" };
                let (d, e) = (r[key].as_f64().unwrap(), s[key].as_f64().unwrap());
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

fn no_derived(b: &Value) {
    for r in b["results"].as_array().unwrap() {
        if let Some(vs) = r.get("vectors") {
            for k in vs.as_object().unwrap().keys() {
                assert!(!k.ends_with(pstore_format::fde::FIELD_SUFFIX), "{r}");
            }
        }
    }
}

#[tokio::test]
async fn rows_answer_only_the_named_fields_they_hold() {
    // Criterion 3's rows.
    let a = api();
    let with = |q: Value, names: Value| {
        let mut q = q;
        q["include_vectors"] = names;
        q
    };
    // Folded rows with no named field answer no `vectors` key.
    let plain: Vec<Value> = (0..5)
        .map(|i| json!({"id": format!("p{i}"), "vector": [1.0, 0.5, f64::from(i), 0.25]}))
        .collect();
    write(&a, "u", json!({"documents": plain})).await;
    fold(&a).await;
    // Unfolded: one row with `title` alone, one with none.
    write(
        &a,
        "u",
        json!({"documents": [
            {"id": "t", "vector": [1.0, 0.0, 0.0, 0.0], "vectors": {"title": [[0.5, 0.25]]}},
            {"id": "n", "vector": [0.0, 1.0, 0.0, 0.0]},
        ]}),
    )
    .await;
    for q in [
        json!({"vector": [1.0, 1.0, 1.0, 1.0], "top_k": 10, "exact": true}),
        json!({"rank_by": ["id", "asc"], "top_k": 10}),
    ] {
        let b = query(
            &a,
            "u",
            &with(q.clone(), json!(["title", "tokens", "absent"])),
        )
        .await;
        assert_eq!(ids(&b).len(), 7, "{q}: {b}");
        no_derived(&b);
        for r in b["results"].as_array().unwrap() {
            if r["id"] == "t" {
                assert_eq!(r["vectors"], json!({"title": [[0.5, 0.25]]}), "{q}: {r}");
            } else {
                assert!(r.get("vectors").is_none(), "{q}: {r}");
            }
            // Only `vector` names `vector`.
            assert!(r.get("vector").is_none(), "{q}: {r}");
        }
    }
    // A width-0 index (M170) holds no named field: one needs a `vector` beside it (M61).
    let (st, b) = send(
        &a,
        "PUT",
        "/v1/indexes/w/documents",
        &json!({"documents": [{"id": "f", "text": "alpha", "vectors": {"title": [[1.0, 2.0]]}}]}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    // With `diversify`, the rows it picks answer their `title`.
    let written = rows(0, 40, 3, |_| 0);
    write(&a, "d", json!({"documents": docs(&written)})).await;
    fold(&a).await;
    let b = query(
        &a,
        "d",
        &with(
            json!({"vector": vec![1.0_f32; 8], "top_k": 5,
                "diversify": {"lambda": 0.5, "window": 20}}),
            json!(["title"]),
        ),
    )
    .await;
    let picked = b["results"].as_array().unwrap();
    assert_eq!(picked.len(), 5, "{b}");
    for r in picked {
        assert_named("dot_product", r, &written[r["id"].as_str().unwrap()]);
        // `diversify` reads `vector`, and answers it only when named.
        assert!(r.get("vector").is_none(), "{r}");
    }
}

#[tokio::test]
async fn a_field_folded_at_a_varying_count_fails_naming_the_rule() {
    // Criterion 3: one row without `title` folds it at a varying count.
    let a = api();
    write(
        &a,
        "x",
        json!({"documents": [
            {"id": "a", "vector": [1.0, 0.0], "vectors": {"title": [[1.0]]}},
            {"id": "b", "vector": [0.0, 1.0]},
        ]}),
    )
    .await;
    fold(&a).await;
    for q in [
        json!({"vector": [1.0, 1.0], "top_k": 2, "include_vectors": ["title"]}),
        json!({"rank_by": ["id", "asc"], "top_k": 2, "include_vectors": ["title"]}),
    ] {
        let (s, b) = ask(&a, "x", &q).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{q}: {b}");
        let msg = b["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("`title` is folded at a varying count of vectors a row"),
            "{q}: {b}"
        );
    }
    // A folded multi-vector field is always of the varying layout.
    write(
        &a,
        "m",
        json!({"documents": [
            {"id": "a", "vector": [1.0, 0.0], "vectors": {"tokens": [[1.0], [2.0]]}},
        ]}),
    )
    .await;
    fold(&a).await;
    let (s, b) = ask(
        &a,
        "m",
        &json!({"rank_by": ["id", "asc"], "top_k": 1, "include_vectors": ["tokens"]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("`tokens` is folded at a varying count of vectors a row"),
        "{b}"
    );
    // `vector` alone still projects.
    let b = query(
        &a,
        "x",
        &json!({"rank_by": ["id", "asc"], "top_k": 2, "include_vectors": ["vector"]}),
    )
    .await;
    assert_eq!(ids(&b), ["a", "b"], "{b}");
}

#[tokio::test]
async fn names_that_cannot_be_projected_are_refused_naming_the_rule() {
    // Criterion 3's refusals.
    let a = api();
    write(&a, "r", json!({"documents": docs(&rows(0, 5, 1, |_| 2))})).await;
    let ranked = json!({"vector": vec![1.0_f32; 8], "top_k": 3});
    let names = |n: usize| -> Vec<String> {
        std::iter::once("vector".to_owned())
            .chain((1..n).map(|i| format!("f{i}")))
            .collect()
    };
    let cases = [
        (json!([""]), "a named vector field's name is 1 to 64 bytes"),
        (
            json!(["x".repeat(65)]),
            "a named vector field's name is 1 to 64 bytes",
        ),
        (json!(["a\u{0}b"]), "may not contain a NUL byte"),
        (json!(["title$fde"]), "may not end `$fde`"),
        (json!(["sparse"]), "`sparse` is not projected"),
        (json!(["title", "title"]), "\"title\" is named twice"),
        (json!(["vector", "vector"]), "\"vector\" is named twice"),
        (json!([1]), "1 is not a vector field's name"),
        (json!([]), "a list of the vector fields"),
        (json!("title"), "a list of the vector fields"),
        (json!(names(10)), "10 names: at most 9 fields"),
    ];
    for (names, why) in &cases {
        let mut q = ranked.clone();
        q["include_vectors"] = names.clone();
        let (s, b) = ask(&a, "r", &q).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{q}: {b}");
        let msg = b["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("include_vectors"), "{q}: {b}");
        assert!(msg.contains(why), "{q}: {b}");
    }
    // Nine are admitted, and a name the index has no field of answers nothing.
    let mut q = ranked.clone();
    q["include_vectors"] = json!(names(9));
    let b = query(&a, "r", &q).await;
    assert_eq!(ids(&b).len(), 3, "{b}");
    for r in b["results"].as_array().unwrap() {
        assert!(r.get("vectors").is_none(), "{r}");
    }
}

/// One cold query's `(depth, requests)` over an index of `segments` folded segments, every
/// row holding `title`.
async fn cost(segments: usize, q: &Value) -> (usize, usize) {
    let store = DepthCounting::new(MemoryStore::new());
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    for s in 0..segments {
        let part: BTreeMap<String, Row> = rows(0, 20, 100 + s as u64, |_| 0)
            .into_iter()
            .map(|(id, r)| (format!("{id}-{s}"), r))
            .collect();
        write(&a, "c", json!({"documents": docs(&part)})).await;
        fold(&a).await;
    }
    let cold = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
    store.reset();
    let b = query(&cold, "c", q).await;
    assert_eq!(ids(&b).len(), 10, "{q}: {b}");
    (store.depth(), store.requests())
}

#[tokio::test]
async fn a_named_projection_costs_no_round_and_a_request_a_segment() {
    // Criterion 4, at 1 and at 4 segments.
    let ranked = json!({"vector": vec![1.0_f32; 8], "top_k": 10, "exact": true});
    let ordered = json!({"rank_by": ["id", "asc"], "top_k": 10});
    let with = |q: &Value, names: Value| {
        let mut q = q.clone();
        q["include_vectors"] = names;
        q
    };
    for (segments, pinned) in [(1, PINNED_1), (4, PINNED_4)] {
        let rv = cost(segments, &with(&ranked, json!(["vector"]))).await;
        let rn = cost(segments, &with(&ranked, json!(["vector", "title"]))).await;
        let ov = cost(segments, &with(&ordered, json!(["vector"]))).await;
        let on = cost(segments, &with(&ordered, json!(["vector", "title"]))).await;
        assert_eq!(
            rn.0, rv.0,
            "{segments}: a ranked named projection added a round"
        );
        assert_eq!(on.0, 4, "{segments}: an order's projection: {on:?}");
        // The order's ten rows lie in every segment: `title` is one more request each.
        assert_eq!(on.1, ov.1 + segments, "{segments}: {on:?} vs {ov:?}");
        assert!(rn.1 <= rv.1 + segments, "{segments}: {rn:?} vs {rv:?}");
        assert_eq!([rv, rn, ov, on], pinned, "{segments} segments");
    }
}

/// `(depth, requests)` of a ranked query projecting `vector`, it and `title`, an order
/// projecting `vector`, and it and `title`, at one segment. `vector` alone is M179's pins.
const PINNED_1: [(usize, usize); 4] = [(4, 5), (4, 6), (4, 4), (4, 5)];
/// The same at four segments: the ten rows lie in every segment.
const PINNED_4: [(usize, usize); 4] = [(4, 17), (4, 21), (4, 13), (4, 17)];
