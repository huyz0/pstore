//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Sparse vectors in results and exports (M193): each row's pairs, read from the segment's
//! `SparseRows` section in M191's extra round, under the same bound.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_format::sparse::{ImpactEncoding, read_list, write_list};
use pstore_server::Api;
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
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
        .header("x-pstore-tenant", "193")
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

/// `w` as an f16 weight answers it, through the format's own encoding.
fn f16(w: f32) -> f32 {
    let mut raw = Vec::new();
    write_list(&mut raw, &[(0, w)], ImpactEncoding::F16, 0.0);
    read_list(&raw, 1, ImpactEncoding::F16, 0.0)[0].1
}

/// A row as written.
#[derive(Clone)]
struct Row {
    vector: Vec<f32>,
    sparse: BTreeMap<String, f32>,
    tokens: Vec<Vec<f32>>,
}

/// One of 2,000 keys, 2 to 24 bytes long.
fn key(n: u64) -> String {
    let mut k = format!("k{n}");
    k.push_str(&"x".repeat(usize::try_from(n % 20).expect("below 20")));
    k
}

/// Rows `from..to`: `vector` (width 8), `sparse` of `keys(i)` keys (weights with zeros of
/// both signs and negatives), and 1 to 5 `tokens` (width 4).
fn rows(from: usize, to: usize, seed: u64, keys: impl Fn(usize) -> usize) -> BTreeMap<String, Row> {
    let mut next = rng(seed);
    (from..to)
        .map(|i| {
            let mut sparse = BTreeMap::new();
            for j in 0..keys(i) {
                let w = match (i + j) % 11 {
                    0 => 0.0,
                    1 => -0.0,
                    _ => vector(&mut next, 1)[0] * 3.0,
                };
                sparse.insert(key(next() % 2_000), w);
            }
            let tokens = (0..1 + i % 5).map(|_| vector(&mut next, 4)).collect();
            (
                format!("r{i:04}"),
                Row {
                    vector: vector(&mut next, 8),
                    sparse,
                    tokens,
                },
            )
        })
        .collect()
}

fn docs(rows: &BTreeMap<String, Row>) -> Vec<Value> {
    rows.iter()
        .map(|(id, r)| {
            let mut d = json!({"id": id, "vector": r.vector, "vectors": {"tokens": r.tokens},
                "attributes": {"n": id}});
            if !r.sparse.is_empty() {
                d["sparse"] = json!(r.sparse);
            }
            d
        })
        .collect()
}

/// A result's `sparse`, each weight as f32 bits; `None` when it has none.
fn sparse_of(r: &Value) -> Option<BTreeMap<String, u32>> {
    let m = r
        .get("sparse")?
        .as_object()
        .unwrap_or_else(|| panic!("{r}"));
    Some(
        m.iter()
            .map(|(k, w)| {
                #[expect(clippy::cast_possible_truncation, reason = "an f32's spelling")]
                let w = w.as_f64().unwrap() as f32;
                (k.clone(), w.to_bits())
            })
            .collect(),
    )
}

/// What a row's `sparse` answers: its weights rounded to f16, as bits.
fn stored(r: &Row) -> Option<BTreeMap<String, u32>> {
    (!r.sparse.is_empty()).then(|| {
        r.sparse
            .iter()
            .map(|(k, w)| (k.clone(), f16(*w).to_bits()))
            .collect()
    })
}

/// 300 rows folded in two segments (every seventh without `sparse`), 50 more unfolded, read
/// by an API restarted over the same store.
async fn built() -> (A<MemoryStore>, BTreeMap<String, Row>) {
    let store = MemoryStore::new();
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    let keys = |i: usize| if i % 7 == 3 { 0 } else { i % 41 };
    let mut all = BTreeMap::new();
    for (from, to, seed) in [(0, 150, 1), (150, 300, 2)] {
        let part = rows(from, to, seed, keys);
        write(&a, "s", json!({"documents": docs(&part)})).await;
        fold(&a).await;
        all.extend(part);
    }
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
    let fresh = rows(300, 350, 3, keys);
    write(&a, "s", json!({"documents": docs(&fresh)})).await;
    all.extend(fresh);
    (a, all)
}

#[tokio::test]
async fn a_ranked_query_answers_each_rows_sparse_pairs_as_stored() {
    // Criterion 1.
    let (a, rows) = built().await;
    let mut next = rng(9);
    for n in 0..10 {
        let q = vector(&mut next, 8);
        let names = json!(["vector", "tokens", "sparse"]);
        let float = query(
            &a,
            "s",
            &json!({"vector": q, "top_k": 80, "exact": true,
            "include_vectors": names}),
        )
        .await;
        let b64 = query(
            &a,
            "s",
            &json!({"vector": q, "top_k": 80, "exact": true,
            "include_vectors": names, "vector_encoding": "base64"}),
        )
        .await;
        let results = float["results"].as_array().unwrap();
        assert_eq!(results.len(), 80, "{n}");
        assert!(results.iter().any(|r| r["id"].as_str().unwrap() >= "r0300"));
        assert!(results.iter().any(|r| r["id"].as_str().unwrap() < "r0150"));
        assert!(
            results.iter().any(|r| r.get("sparse").is_none()),
            "a row without it"
        );
        for (f, s) in results.iter().zip(b64["results"].as_array().unwrap()) {
            let id = f["id"].as_str().unwrap();
            assert_eq!(sparse_of(f), stored(&rows[id]), "{n} {id}: {f}");
            assert_eq!(sparse_of(s), sparse_of(f), "{n} {id}: base64");
            assert_eq!(
                f["vectors"]["tokens"].as_array().unwrap().len(),
                rows[id].tokens.len(),
                "{id}"
            );
            assert_eq!(decoded(&f["vector"]).len(), 8, "{id}");
        }
    }
    // A signed zero answers as one.
    let zero = rows
        .iter()
        .find(|(_, r)| {
            r.sparse
                .values()
                .any(|w| w.to_bits() == (-0.0_f32).to_bits())
        })
        .unwrap();
    let b = query(
        &a,
        "s",
        &json!({"rank_by": ["id", "asc"], "top_k": 1,
        "filters": ["n", "Eq", zero.0], "include_vectors": ["sparse"]}),
    )
    .await;
    assert_eq!(sparse_of(&b["results"][0]), stored(zero.1), "{b}");
}

/// The rows of `index` exported by id, 64 a page, projecting `names`.
async fn export<S: pstore_blob::BlobStore + 'static>(
    a: &A<S>,
    index: &str,
    names: &Value,
) -> Vec<Value> {
    let mut out = Vec::new();
    loop {
        let b = query(
            a,
            index,
            &json!({"rank_by": ["id", "asc"], "top_k": 64,
            "offset": out.len(), "include_vectors": names}),
        )
        .await;
        let page = b["results"].as_array().unwrap().clone();
        let done = page.len() < 64;
        out.extend(page);
        if done {
            return out;
        }
    }
}

/// `(score, id)` of each result, ids sorted within a score: a tie breaks by segment and row,
/// which a rewritten index folds differently.
fn by_score(b: &Value) -> Vec<(u32, Vec<String>)> {
    let mut out: Vec<(u32, Vec<String>)> = Vec::new();
    for r in b["results"].as_array().unwrap() {
        #[expect(clippy::cast_possible_truncation, reason = "an f32's spelling")]
        let s = (r["score"].as_f64().or_else(|| r["$dist"].as_f64()).unwrap() as f32).to_bits();
        let id = r["id"].as_str().unwrap().to_owned();
        match out.last_mut() {
            Some((last, ids)) if *last == s => ids.push(id),
            _ => out.push((s, vec![id])),
        }
    }
    for (_, ids) in &mut out {
        ids.sort();
    }
    // The last group may be cut by `top_k` differently: compare it only by score.
    if let Some((_, ids)) = out.last_mut() {
        ids.clear();
    }
    out
}

#[tokio::test]
async fn an_export_by_id_pages_rebuilds_the_sparse_field_exactly() {
    // Criterion 2.
    let (a, rows) = built().await;
    let got = export(&a, "s", &json!(["vector", "sparse"])).await;
    assert_eq!(got.len(), rows.len());
    let docs: Vec<Value> = got
        .iter()
        .map(|r| {
            let id = r["id"].as_str().unwrap();
            assert_eq!(sparse_of(r), stored(&rows[id]), "{id}");
            let mut d = json!({"id": id, "vector": r["vector"]});
            if let Some(s) = r.get("sparse") {
                d["sparse"] = s.clone();
            }
            d
        })
        .collect();
    write(&a, "copy", json!({"documents": docs})).await;
    fold(&a).await;
    let mut next = rng(21);
    for n in 0..20 {
        let q: BTreeMap<String, f32> = (0..8)
            .map(|_| (key(next() % 2_000), vector(&mut next, 1)[0]))
            .collect();
        let body = json!({"sparse": q, "top_k": 20});
        let (x, y) = (query(&a, "s", &body).await, query(&a, "copy", &body).await);
        assert_eq!(by_score(&x), by_score(&y), "sparse {n}");
        let body = json!({"vector": vector(&mut next, 8), "top_k": 20, "exact": true});
        let (x, y) = (query(&a, "s", &body).await, query(&a, "copy", &body).await);
        assert_eq!(by_score(&x), by_score(&y), "vector {n}");
    }
    // A sparse-only index (M170) exports its pairs the same way.
    let only: Vec<Value> = rows
        .iter()
        .filter(|(_, r)| !r.sparse.is_empty())
        .take(20)
        .map(|(id, r)| json!({"id": id, "sparse": r.sparse}))
        .collect();
    write(&a, "only", json!({"documents": only})).await;
    fold(&a).await;
    let got = export(&a, "only", &json!(["sparse"])).await;
    assert_eq!(got.len(), 20);
    for r in &got {
        let id = r["id"].as_str().unwrap();
        assert_eq!(sparse_of(r), stored(&rows[id]), "{id}");
    }
}

/// The bytes a row's pairs and `tokens` hold as stored: `2 + key + 2` a pair, 16 a vector.
fn bytes(r: &Row) -> (u64, u64) {
    let pairs = r.sparse.keys().map(|k| 4 + k.len() as u64).sum();
    (pairs, 16 * r.tokens.len() as u64)
}

/// One cold query's `(depth, requests)` and answer over `segments` folded segments of 20
/// rows, each with 1 to 9 keys, under a bound of `bound` bytes.
async fn cost(
    segments: usize,
    bound: Option<u64>,
    q: &Value,
) -> ((usize, usize), StatusCode, Value, BTreeMap<String, Row>) {
    let store = DepthCounting::new(MemoryStore::new());
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    let mut all = BTreeMap::new();
    for s in 0..segments {
        let part: BTreeMap<String, Row> = rows(0, 20, 100 + s as u64, |i| 1 + i % 9)
            .into_iter()
            .map(|(id, r)| (format!("{id}-{s}"), r))
            .collect();
        write(&a, "c", json!({"documents": docs(&part)})).await;
        fold(&a).await;
        all.extend(part);
    }
    let cold = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
    if let Some(b) = bound {
        cold.set_projection_bytes_for_test(b);
    }
    store.reset();
    let (s, b) = ask(&cold, "c", q).await;
    ((store.depth(), store.requests()), s, b, all)
}

fn with(q: &Value, names: Value) -> Value {
    let mut q = q.clone();
    q["include_vectors"] = names;
    q
}

#[tokio::test]
async fn the_pairs_count_toward_the_bound_with_the_varying_fields() {
    // Criterion 3: the bound, over a ranked query and an order.
    let names = json!(["tokens", "sparse"]);
    for q in [
        json!({"vector": vec![1.0_f32; 8], "top_k": 10, "exact": true}),
        json!({"rank_by": ["id", "asc"], "top_k": 10}),
    ] {
        let q = with(&q, names.clone());
        let (admitted, s, b, rows) = cost(1, None, &q).await;
        assert_eq!(s, StatusCode::OK, "{q}: {b}");
        let (pairs, tokens): (u64, u64) = ids(&b)
            .iter()
            .map(|id| bytes(&rows[id]))
            .fold((0, 0), |(p, t), (x, y)| (p + x, t + y));
        assert!(pairs > 0 && tokens > 0, "{b}");
        let sum = pairs + tokens;
        let (refused, s, b, _) = cost(1, Some(sum - 1), &q).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{q}: {b}");
        let msg = b["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains(&format!(
                "would read {sum} bytes, more than the {}",
                sum - 1
            )),
            "{q}: {b}"
        );
        assert_eq!(
            refused.0 + 1,
            admitted.0,
            "{q}: no pairs round when refused"
        );
        let (_, s, b, _) = cost(1, Some(sum), &q).await;
        assert_eq!(s, StatusCode::OK, "{q}: {b}");
    }
}

#[tokio::test]
async fn a_segment_folded_without_the_section_refuses_until_rewritten() {
    // Criterion 3: a segment as a build before M193 folded it.
    let store = MemoryStore::new();
    let old = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    old.set_sparse_rows_for_test(false);
    let first = rows(0, 30, 5, |i| if i % 3 == 0 { 0 } else { 1 + i % 4 });
    write(&old, "o", json!({"documents": docs(&first)})).await;
    fold(&old).await;
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
    let held = first.iter().find(|(_, r)| !r.sparse.is_empty()).unwrap().0;
    let none = first.iter().find(|(_, r)| r.sparse.is_empty()).unwrap().0;
    for id in [held, none] {
        let q = json!({"rank_by": ["id", "asc"], "top_k": 1, "filters": ["n", "Eq", id],
            "include_vectors": ["sparse"]});
        let (s, b) = ask(&a, "o", &q).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{id}: {b}");
        let msg = b["error"]["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains("`sparse`") && msg.contains("folded before"),
            "{id}: {b}"
        );
        let q = with(
            &json!({"vector": rows_vector(&first[id]), "top_k": 1, "exact": true,
            "filters": ["n", "Eq", id]}),
            json!(["sparse"]),
        );
        let (s, b) = ask(&a, "o", &q).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{id}: ranked {b}");
        // Not projecting it answers as before.
        let b = query(&a, "o", &with(&q, json!(["vector"]))).await;
        assert_eq!(ids(&b), [id.as_str()]);
    }
    // A segment written after M193, then a compaction of both by this build.
    let second = rows(30, 40, 6, |i| 1 + i % 4);
    write(&a, "o", json!({"documents": docs(&second)})).await;
    fold(&a).await;
    let tenant = TenantId(193);
    let engine = pstore_engine::Engine::new(
        Arc::new(Accounted::new(store.clone()).as_tenant(tenant)),
        tenant,
        LaneId(9),
    );
    assert!(
        engine.compact("o").await.unwrap().is_some(),
        "nothing compacted"
    );
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(3)).unwrap();
    let got = export(&a, "o", &json!(["sparse"])).await;
    assert_eq!(got.len(), 40);
    for r in &got {
        let id = r["id"].as_str().unwrap();
        let row = first.get(id).or_else(|| second.get(id)).unwrap();
        assert_eq!(sparse_of(r), stored(row), "{id}");
    }
    // A name twice is refused, as every name is; `sparse` is one name past M190's nine.
    let (s, b) = ask(
        &a,
        "o",
        &json!({"rank_by": ["id", "asc"], "top_k": 1,
        "include_vectors": ["sparse", "sparse"]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let names = |n: usize| -> Vec<String> {
        ["vector".to_owned(), "sparse".to_owned()]
            .into_iter()
            .chain((1..n).map(|i| format!("f{i}")))
            .collect()
    };
    let q = |n| json!({"rank_by": ["id", "asc"], "top_k": 1, "include_vectors": names(n)});
    let (s, b) = ask(&a, "o", &q(9)).await;
    assert_eq!(s, StatusCode::OK, "ten names: {b}");
    let (s, b) = ask(&a, "o", &q(10)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("include_vectors: 10 names: at most 9 fields"),
        "the dense names counted, `sparse` beside them: {b}"
    );
}

fn rows_vector(r: &Row) -> Vec<f32> {
    r.vector.clone()
}

#[tokio::test]
async fn projecting_sparse_costs_the_same_one_round_more() {
    // Criterion 4, at 1 and at 4 segments.
    let ranked = json!({"vector": vec![1.0_f32; 8], "top_k": 10, "exact": true});
    let ordered = json!({"rank_by": ["id", "asc"], "top_k": 10});
    for (segments, pinned) in [(1, PINNED_1), (4, PINNED_4)] {
        let mut got = Vec::new();
        for (q, names) in [
            (&ranked, json!(["vector"])),
            (&ranked, json!(["sparse"])),
            (&ranked, json!(["tokens", "sparse"])),
            (&ordered, json!(["sparse"])),
            (&ordered, json!(["vector", "sparse"])),
        ] {
            let q = with(q, names);
            let (c, s, b, _) = cost(segments, None, &q).await;
            assert_eq!(s, StatusCode::OK, "{q}: {b}");
            assert_eq!(ids(&b).len(), 10, "{q}: {b}");
            let asked = q["include_vectors"]
                .as_array()
                .unwrap()
                .contains(&json!("sparse"));
            assert_eq!(b["results"][0].get("sparse").is_some(), asked, "{q}: {b}");
            got.push(c);
        }
        assert_eq!(got[1].0, got[0].0 + 1, "{segments}: one round more");
        assert_eq!(
            got[2].0, got[1].0,
            "{segments}: shared with the varying fields"
        );
        assert_eq!(got[3].0, 5, "{segments}: an order");
        assert_eq!(got[4].0, 5, "{segments}: an order with `vector`");
        assert_eq!(got, pinned, "{segments} segments");
    }
}

/// `(depth, requests)` of a ranked query naming `vector`, `sparse`, and `tokens` with
/// `sparse`, then an order naming `sparse` and one naming `vector` and `sparse`, at one
/// segment, measured on the first run.
const PINNED_1: [(usize, usize); 5] = [(4, 5), (5, 6), (5, 7), (5, 5), (5, 6)];
/// The same at four segments.
const PINNED_4: [(usize, usize); 5] = [(4, 17), (5, 21), (5, 25), (5, 17), (5, 21)];
