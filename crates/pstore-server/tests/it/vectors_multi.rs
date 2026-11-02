//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Folded multi-vector fields in results and exports (M191): a named field a segment stores
//! at a varying count of vectors a row, read in one round more, under a bound on its bytes.

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
        .header("x-pstore-tenant", "191")
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

/// Rows `from..to`, each with `vector` (width 8), with `title` (one vector of width 4) where
/// `title(i)`, and with `tokens` (width 4) of `tokens(i)` vectors where that is not 0.
fn rows(
    from: usize,
    to: usize,
    seed: u64,
    title: impl Fn(usize) -> bool,
    tokens: impl Fn(usize) -> usize,
) -> BTreeMap<String, Row> {
    let mut next = rng(seed);
    (from..to)
        .map(|i| {
            let mut named = BTreeMap::new();
            let t = vector(&mut next, 4);
            if title(i) {
                named.insert("title".to_owned(), vec![t]);
            }
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

/// 300 rows with 1 to 5 `tokens` folded in two segments -- the first with `title` on every
/// row, so fixed there, the second without it on every fifth, so varying there -- and 50
/// more unfolded, under `metric`, read back by an API restarted over the same store.
async fn built(metric: &str) -> (A<MemoryStore>, BTreeMap<String, Row>) {
    let store = Accounted::new(MemoryStore::new());
    let a = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    let first = rows(0, 150, 190, |_| true, |i| 1 + i % 5);
    let second = rows(150, 300, 191, |i| i % 5 != 0, |i| 1 + i % 5);
    let fresh = rows(300, 350, 192, |_| true, |i| 1 + i % 5);
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
async fn a_ranked_query_answers_folded_multi_vector_fields_in_either_spelling() {
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
            // Some row of the varying segment lacks `title`, and answers no entry.
            assert!(results.iter().any(|r| named_of(r, "title").is_none()));
            for (f, s) in results.iter().zip(b64["results"].as_array().unwrap()) {
                let id = f["id"].as_str().unwrap();
                assert!(s["vectors"]["tokens"][0].is_string(), "{s}");
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
async fn an_export_by_id_pages_rebuild_folded_multi_vector_fields_exactly() {
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

/// One cold query's `(depth, requests)` and answer over `segments` folded segments of 20
/// rows, each with `title` on every row and 1 to 5 `tokens`, under a projection bound of
/// `bound` bytes, with `unfolded` more rows written after the fold.
async fn cost(
    segments: usize,
    unfolded: usize,
    bound: Option<u64>,
    q: &Value,
) -> ((usize, usize), StatusCode, Value) {
    let store = DepthCounting::new(MemoryStore::new());
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    for s in 0..segments {
        let part: BTreeMap<String, Row> = rows(0, 20, 100 + s as u64, |_| true, |i| 1 + i % 5)
            .into_iter()
            .map(|(id, r)| (format!("{id}-{s}"), r))
            .collect();
        write(&a, "c", json!({"documents": docs(&part)})).await;
        fold(&a).await;
    }
    let cold = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
    if let Some(b) = bound {
        cold.set_projection_bytes_for_test(b);
    }
    if unfolded > 0 {
        let fresh: BTreeMap<String, Row> = rows(0, unfolded, 7, |_| true, |_| 0)
            .into_iter()
            .map(|(id, r)| (format!("u{id}"), r))
            .collect();
        write(&cold, "c", json!({"documents": docs(&fresh)})).await;
    }
    store.reset();
    let (s, b) = ask(&cold, "c", q).await;
    ((store.depth(), store.requests()), s, b)
}

fn with(q: &Value, names: Value) -> Value {
    let mut q = q.clone();
    q["include_vectors"] = names;
    q
}

#[tokio::test]
async fn projected_bytes_past_the_bound_are_refused_before_any_vector_is_read() {
    // Criterion 3. `tokens` of width 4 are 16 bytes a vector stored (dot product), 1 to 5 a
    // row: a page of 10 rows holds 30 vectors, 480 bytes, past 300; 5 rows are at most 400.
    let ranked = json!({"vector": vec![1.0_f32; 8], "top_k": 10, "exact": true});
    let ordered = json!({"rank_by": ["id", "asc"], "top_k": 10});
    for q in [&ranked, &ordered] {
        let q = with(q, json!(["tokens"]));
        let (admitted, s, b) = cost(1, 0, None, &q).await;
        assert_eq!(s, StatusCode::OK, "{q}: {b}");
        // The refusal below is for these hits' bytes, whichever rows rank.
        assert!(token_bytes(&b) > 300, "{q}: {b}");
        let (refused, s, b) = cost(1, 0, Some(300), &q).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{q}: {b}");
        let msg = b["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("bytes, more than the 300 a query may project"),
            "{q}: {b}"
        );
        assert_eq!(
            refused.0 + 1,
            admitted.0,
            "{q}: no vector round when refused"
        );
        let mut five = q.clone();
        five["top_k"] = json!(5);
        let (_, s, b) = cost(1, 0, Some(10_000), &five).await;
        assert_eq!(s, StatusCode::OK, "{five}: {b}");
        assert_eq!(ids(&b).len(), 5, "{b}");
    }
    // With something unfolded the row fetch reads every leg's candidates, but the bound
    // counts the hits only: the page fits (2 rows, at most 160 bytes), the candidates of 4
    // segments would not.
    let q = with(
        &json!({"vector": vec![1.0_f32; 8], "top_k": 2, "exact": true}),
        json!(["tokens"]),
    );
    let (_, s, b) = cost(4, 3, Some(200), &q).await;
    assert_eq!(s, StatusCode::OK, "{q}: {b}");
    assert_eq!(ids(&b).len(), 2, "{b}");
    // The hits are folded rows whose `tokens` the bound counted, not unfolded rows with none.
    assert!(ids(&b).iter().all(|id| !id.starts_with('u')), "{b}");
    assert!((1..=200).contains(&token_bytes(&b)), "{b}");
}

/// The bytes the answered rows' `tokens` hold as stored under dot product: 16 a vector.
fn token_bytes(b: &Value) -> usize {
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["vectors"]["tokens"].as_array().map_or(0, Vec::len) * 16)
        .sum()
}

#[tokio::test]
async fn the_bound_counts_every_segments_and_fields_bytes_and_admits_its_equal() {
    // Criterion 3. Two segments of 20 rows, each varying in `title` (every fifth row lacks
    // it) and in `tokens` (1 to 5 a row): under dot product a width-4 vector is 16 bytes
    // stored, so a segment holds 60 `tokens` and 16 `title`s, 1,216 bytes, and both 2,432.
    let store = MemoryStore::new();
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    for s in 0..2u64 {
        let part: BTreeMap<String, Row> = rows(0, 20, 300 + s, |i| i % 5 != 0, |i| 1 + i % 5)
            .into_iter()
            .map(|(id, r)| (format!("{id}-{s}"), r))
            .collect();
        write(&a, "b", json!({"documents": docs(&part)})).await;
        fold(&a).await;
    }
    let names = json!(["title", "tokens"]);
    for q in [
        json!({"vector": vec![1.0_f32; 8], "top_k": 40, "exact": true}),
        json!({"rank_by": ["id", "asc"], "top_k": 40}),
    ] {
        let q = with(&q, names.clone());
        for (bound, want) in [
            (2_431, StatusCode::UNPROCESSABLE_ENTITY),
            (2_432, StatusCode::OK),
        ] {
            // The bound is the engine's, set before the tenant's first request.
            let cold = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
            cold.set_projection_bytes_for_test(bound);
            let (s, b) = ask(&cold, "b", &q).await;
            assert_eq!(s, want, "{bound} {q}: {b}");
            if s == StatusCode::OK {
                assert_eq!(ids(&b).len(), 40, "{b}");
            } else {
                let msg = b["error"]["message"].as_str().unwrap_or_default();
                assert!(
                    msg.contains("would read 2432 bytes, more than the 2431 a query"),
                    "{q}: {b}"
                );
            }
        }
    }
}

#[tokio::test]
async fn a_folded_multi_vector_projection_costs_one_round_more() {
    // Criterion 4, at 1 and at 4 segments.
    let ranked = json!({"vector": vec![1.0_f32; 8], "top_k": 10, "exact": true});
    let ordered = json!({"rank_by": ["id", "asc"], "top_k": 10});
    for (segments, pinned) in [(1, PINNED_1), (4, PINNED_4)] {
        let mut got = Vec::new();
        for (q, names) in [
            (&ranked, json!(["vector"])),
            (&ranked, json!(["vector", "tokens"])),
            (&ranked, json!(["title"])),
            (&ordered, json!(["vector"])),
            (&ordered, json!(["vector", "tokens"])),
            (&ordered, json!(["title"])),
        ] {
            let q = with(q, names);
            let (c, s, b) = cost(segments, 0, None, &q).await;
            assert_eq!(s, StatusCode::OK, "{q}: {b}");
            assert_eq!(ids(&b).len(), 10, "{q}: {b}");
            got.push(c);
        }
        assert_eq!(got[1].0, got[0].0 + 1, "{segments}: ranked, one round more");
        assert_eq!(
            got[2].0, got[0].0,
            "{segments}: a fixed `title` costs no round"
        );
        assert_eq!(got[4].0, 5, "{segments}: an order naming `tokens`");
        assert_eq!(got[5].0, 4, "{segments}: an order naming a fixed `title`");
        assert_eq!(got, pinned, "{segments} segments");
    }
}

/// `(depth, requests)` of a ranked query naming `vector`, it and `tokens`, `title` alone,
/// then the same three for an order, at one segment, measured on the first run and pinned.
const PINNED_1: [(usize, usize); 6] = [(4, 5), (5, 7), (4, 5), (4, 4), (5, 6), (4, 4)];
/// The same at four segments.
const PINNED_4: [(usize, usize); 6] = [(4, 17), (5, 25), (4, 17), (4, 13), (5, 21), (4, 13)];
