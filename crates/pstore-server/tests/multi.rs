//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M61: documents of several vectors in a named field, searched by MaxSim, through the API.
//!
//! Every ranking is checked against the test's own model, computed in `f64` from what was
//! written: cosine MaxSim over normalized vectors, and under euclidean the Chamfer distance
//! `Σᵢ minⱼ ‖qᵢ − dⱼ‖²` ascending. The fixture asserts its own scores are far enough apart
//! that `f32` rounding cannot reorder them.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

const TENANT: u128 = 14;

fn api(store: &MemoryStore) -> Arc<Api<MemoryStore>> {
    Api::new(Accounted::new(store.clone()), LaneId(1)).expect("a fencing backend")
}

async fn send<S: pstore_blob::BlobStore + Clone + 'static>(
    api: &Arc<Api<S>>,
    method: &str,
    uri: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT.to_string())
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn put(api: &Arc<Api<MemoryStore>>, index: &str, body: &Value) -> (StatusCode, Value) {
    send(
        api,
        "PUT",
        &format!("/v1/indexes/{index}/documents"),
        Some(body),
    )
    .await
}

async fn query(api: &Arc<Api<MemoryStore>>, index: &str, body: &Value) -> (StatusCode, Value) {
    send(
        api,
        "POST",
        &format!("/v1/indexes/{index}/query"),
        Some(body),
    )
    .await
}

async fn fold(api: &Arc<Api<MemoryStore>>) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn ids(body: &Value) -> Vec<String> {
    body["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results: {body}"))
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Row `i`'s `late` vectors: between none and five, 3 dimensions, norms spread over 7x so a
/// skipped normalization or a missing augmentation reorders the answer.
fn late(i: usize) -> Vec<Vec<f32>> {
    let scale = 1.0 + (i % 7) as f32;
    (0..i % 6)
        .map(|j| {
            let x = (i * 5 + j * 3) as f32;
            vec![
                x.sin() * scale,
                (x * 0.7).cos() * scale,
                (x * 0.3).sin() * scale,
            ]
        })
        .collect()
}

/// A live document as the model holds it.
#[derive(Clone)]
struct Row {
    n: i64,
    late: Vec<Vec<f32>>,
}

fn doc(id: &str, n: i64, late: &[Vec<f32>]) -> Value {
    let x = n as f32;
    let mut d = json!({
        "id": id,
        "vector": [x.cos(), x.sin(), 1.0, 0.5],
        "text": format!("word{} common", n % 4),
        "attributes": {"n": n},
    });
    if !late.is_empty() {
        d["vectors"] = json!({ "late": late });
    }
    d
}

const Q: [[f32; 3]; 3] = [[0.2, -0.5, 0.9], [-3.5, 0.5, 1.5], [0.4, 0.4, -0.1]];

fn multi_q() -> Value {
    json!({"field": "late", "vectors": Q})
}

#[derive(Clone, Copy, PartialEq)]
enum Metric {
    Cosine,
    Euclidean,
}

impl Metric {
    fn name(self) -> &'static str {
        match self {
            Self::Cosine => "cosine_distance",
            Self::Euclidean => "euclidean_squared",
        }
    }
}

fn unit(v: &[f32]) -> Vec<f64> {
    let n: f64 = v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    v.iter().map(|x| f64::from(*x) / n).collect()
}

/// The model: every live row with vectors, best first, with the score it is ranked by.
fn model(rows: &BTreeMap<String, Row>, metric: Metric, keep: impl Fn(&Row) -> bool) -> Vec<String> {
    let mut scored: Vec<(String, f64)> = rows
        .iter()
        .filter(|(_, r)| !r.late.is_empty() && keep(r))
        .map(|(id, r)| {
            let score: f64 = Q
                .iter()
                .map(|q| match metric {
                    Metric::Cosine => r
                        .late
                        .iter()
                        .map(|d| unit(q).iter().zip(unit(d)).map(|(a, b)| a * b).sum::<f64>())
                        .fold(f64::NEG_INFINITY, f64::max),
                    // Chamfer, negated, so higher is better in both.
                    Metric::Euclidean => -r
                        .late
                        .iter()
                        .map(|d| {
                            q.iter()
                                .zip(d)
                                .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
                                .sum::<f64>()
                        })
                        .fold(f64::INFINITY, f64::min),
                })
                .sum();
            (id.clone(), score)
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    for w in scored.windows(2) {
        assert!(
            (w[0].1 - w[1].1).abs() > 1e-4 * w[0].1.abs().max(1.0),
            "the fixture has near-tied scores: {} {} and {} {}",
            w[0].0,
            w[0].1,
            w[1].0,
            w[1].1
        );
    }
    scored.into_iter().map(|(id, _)| id).collect()
}

async fn check(
    api: &Arc<Api<MemoryStore>>,
    index: &str,
    rows: &BTreeMap<String, Row>,
    metric: Metric,
    when: &str,
) {
    let (s, b) = query(api, index, &json!({"multi": multi_q(), "top_k": 100})).await;
    assert_eq!(s, StatusCode::OK, "{when}: {b}");
    assert_eq!(
        ids(&b),
        model(rows, metric, |_| true),
        "{} {when}",
        metric.name()
    );
    // No `$dist`: MaxSim is no distance the metric defines.
    assert!(
        b["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r.get("$dist").is_none()),
        "{when}: {b}"
    );
    let (s, b) = query(
        api,
        index,
        &json!({"multi": multi_q(), "top_k": 100, "filters": ["n", "Lt", 30]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{when}: {b}");
    assert_eq!(
        ids(&b),
        model(rows, metric, |r| r.n < 30),
        "{} {when}, filtered",
        metric.name()
    );
}

/// RRF over the legs' own rankings, each asked for alone: with `top_k` above the corpus every
/// leg is its full ranking, so this is the fused query's model. Ids of equal fused score are
/// compared as a set: their order is `(segment, row)`, which no id shows.
async fn check_fused(api: &Arc<Api<MemoryStore>>, index: &str, when: &str) {
    let dense = json!({"vector": [0.3, 0.9, -0.2, 1.0], "exact": true, "top_k": 100});
    let multi = json!({"multi": multi_q(), "top_k": 100});
    let text = json!({"text": "word1", "top_k": 100});
    let mut acc: BTreeMap<String, f32> = BTreeMap::new();
    for leg in [&dense, &multi, &text] {
        let (s, b) = query(api, index, leg).await;
        assert_eq!(s, StatusCode::OK, "{when}: {b}");
        for (i, id) in ids(&b).into_iter().enumerate() {
            *acc.entry(id).or_insert(0.0) += 1.0 / (60.0 + (i + 1) as f32);
        }
    }
    let mut want: Vec<(String, f32)> = acc.into_iter().collect();
    want.sort_by(|a, b| b.1.total_cmp(&a.1));
    let (s, b) = query(
        api,
        index,
        &json!({"vector": [0.3, 0.9, -0.2, 1.0], "exact": true, "multi": multi_q(),
                "text": "word1", "top_k": 100}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{when}: {b}");
    let got = ids(&b);
    assert_eq!(got.len(), want.len(), "{when}");
    let mut at = 0;
    while at < want.len() {
        let end = (at..want.len())
            .find(|e| want[*e].1.to_bits() != want[at].1.to_bits())
            .unwrap_or(want.len());
        let mut g: Vec<&String> = got[at..end].iter().collect();
        let mut w: Vec<&String> = want[at..end].iter().map(|(id, _)| id).collect();
        g.sort();
        w.sort();
        assert_eq!(g, w, "{when}: fused ranks {at}..{end}");
        at = end;
    }
}

async fn compact(store: &MemoryStore, index: &str) {
    let tenant = TenantId(TENANT);
    let engine = pstore_engine::Engine::new(
        Arc::new(Accounted::new(store.clone()).as_tenant(tenant)),
        tenant,
        LaneId(9),
    );
    let epoch = engine.compact(index).await.unwrap();
    assert!(epoch.is_some(), "nothing compacted");
}

/// Rows `range`, written and recorded in the model.
async fn write(
    api: &Arc<Api<MemoryStore>>,
    index: &str,
    metric: Metric,
    rows: &mut BTreeMap<String, Row>,
    range: std::ops::Range<usize>,
) {
    let docs: Vec<Value> = range
        .map(|i| {
            let id = format!("d{i:02}");
            rows.insert(
                id.clone(),
                Row {
                    n: i as i64,
                    late: late(i),
                },
            );
            doc(&id, i as i64, &late(i))
        })
        .collect();
    let (s, b) = put(
        api,
        index,
        &json!({"durability": "durable", "documents": docs, "distance_metric": metric.name()}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn multi_vector_documents_are_searched_by_maxsim() {
    for metric in [Metric::Cosine, Metric::Euclidean] {
        let store = MemoryStore::new();
        let api = api(&store);
        let index = metric.name();
        let mut rows: BTreeMap<String, Row> = BTreeMap::new();
        write(&api, index, metric, &mut rows, 0..20).await;
        check(&api, index, &rows, metric, "unfolded").await;
        fold(&api).await;
        check(&api, index, &rows, metric, "folded").await;
        write(&api, index, metric, &mut rows, 20..45).await;
        fold(&api).await;
        // Deletes, and upserts that change `late`: one row's vectors replaced, one row that
        // had none given some, and one that had some given none.
        let deletes = ["d03", "d22", "d31"];
        for d in deletes {
            rows.remove(d);
        }
        let changed = [("d07", late(107)), ("d12", late(103)), ("d25", Vec::new())];
        let mut docs = Vec::new();
        for (id, vs) in &changed {
            let n = rows[*id].n;
            rows.insert(
                (*id).to_owned(),
                Row {
                    n,
                    late: vs.clone(),
                },
            );
            docs.push(doc(id, n, vs));
        }
        let (s, b) = put(
            &api,
            index,
            &json!({"durability": "durable", "documents": docs, "deletes": deletes,
                    "distance_metric": metric.name()}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        check(&api, index, &rows, metric, "deleted and upserted, unfolded").await;
        fold(&api).await;
        check(&api, index, &rows, metric, "deleted and upserted, folded").await;
        check_fused(&api, index, "folded").await;
        compact(&store, index).await;
        check(&api, index, &rows, metric, "compacted").await;
        check_fused(&api, index, "compacted").await;
    }
}

#[tokio::test]
async fn multi_vectors_are_refused_at_the_door() {
    let store = MemoryStore::new();
    let api = api(&store);
    let one = |vectors: Value| json!({"documents": [{"id": "a", "vector": [1.0, 2.0], "vectors": vectors}]});
    let nan = "AADAfwAAgD8="; // NaN, 1.0
    let many = |n: usize| json!(vec![vec![1.0, 2.0]; n]);
    let nine: serde_json::Map<String, Value> =
        (0..9).map(|i| (format!("f{i}"), json!([[1.0]]))).collect();
    let writes = [
        ("an empty name", one(json!({"": [[1.0]]}))),
        ("a 65-byte name", one(json!({ "x".repeat(65): [[1.0]] }))),
        ("`vector`", one(json!({"vector": [[1.0, 2.0]]}))),
        ("no vectors", one(json!({"late": []}))),
        ("1,025 vectors", one(json!({ "late": many(1025) }))),
        ("an empty vector", one(json!({"late": [[1.0], []]}))),
        ("a non-finite vector", one(json!({"late": [nan]}))),
        ("nine fields", one(Value::Object(nine))),
        ("a zero vector under cosine", {
            let mut b = one(json!({"late": [[1.0, 1.0], [0.0, 0.0]]}));
            b["distance_metric"] = json!("cosine_distance");
            b
        }),
    ];
    for (what, body) in &writes {
        let (s, b) = put(&api, "docs", body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{what}: {b}");
    }
    // The limits themselves are accepted: 1,024 vectors, 8 fields, a 64-byte name.
    let eight: serde_json::Map<String, Value> = (0..7)
        .map(|i| (format!("f{i}"), json!([[1.0]])))
        .chain([("y".repeat(64), many(1024))])
        .collect();
    let (s, b) = put(&api, "docs", &one(Value::Object(eight))).await;
    assert_eq!(s, StatusCode::OK, "{b}");

    let (s, b) = put(
        &api,
        "late",
        &json!({"durability": "durable", "documents": [
            {"id": "a", "vector": [1.0, 2.0], "vectors": {"late": [[1.0, 2.0, 3.0]]}}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let q = |extra: Value| {
        let mut b = json!({"multi": {"field": "late", "vectors": [[1.0, 0.0, 0.0]]}});
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    };
    let texts: Vec<String> = (0..pstore_query::MAX_LEGS - 1)
        .map(|i| format!("w{i}"))
        .collect();
    let queries = [
        ("no field", json!({"multi": {"vectors": [[1.0, 0.0, 0.0]]}})),
        (
            "an unknown key",
            json!({"multi": {"field": "late", "vectors": [[1.0]], "k": 1}}),
        ),
        (
            "`vector` as the field",
            json!({"multi": {"field": "vector", "vectors": [[1.0, 2.0]]}}),
        ),
        (
            "an empty name",
            json!({"multi": {"field": "", "vectors": [[1.0]]}}),
        ),
        (
            "no vectors",
            json!({"multi": {"field": "late", "vectors": []}}),
        ),
        (
            "1,025 vectors",
            json!({"multi": {"field": "late", "vectors": vec![vec![1.0, 0.0, 0.0]; 1025]}}),
        ),
        (
            "an empty vector",
            json!({"multi": {"field": "late", "vectors": [[]]}}),
        ),
        (
            "a non-finite vector",
            json!({"multi": {"field": "late", "vectors": [nan]}}),
        ),
        ("rank_by", q(json!({"rank_by": ["n", "asc"]}))),
        (
            "aggregate_by",
            q(json!({"aggregate_by": {"c": ["Count", "id"]}})),
        ),
        ("sum", q(json!({"fusion": {"sum": {}}}))),
        ("max", q(json!({"fusion": {"max": {}}}))),
        ("17 legs", q(json!({"vector": [1.0, 2.0], "text": texts}))),
        (
            "a dense field other than `vector`",
            json!({"vector": [1.0, 2.0, 3.0], "field": "late"}),
        ),
        (
            "two weights for three legs",
            q(json!({"vector": [1.0, 2.0], "text": "w",
            "fusion": {"rrf": {"weights": [1.0, 1.0]}}})),
        ),
    ];
    for (what, body) in &queries {
        let (s, b) = query(&api, "late", body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{what}: {b}");
    }
    // A dense query naming `vector` itself still runs, and so do 16 legs with weights.
    for body in [
        json!({"vector": [1.0, 2.0], "field": "vector"}),
        q(json!({"vector": [1.0, 2.0], "text": texts[..14].to_vec()})),
        q(json!({"vector": [1.0, 2.0], "text": "w",
            "fusion": {"rrf": {"weights": [1.0, 2.0, 0.5]}}})),
        // `sum` and `max` still combine text legs alone.
        json!({"text": "w", "fusion": {"sum": {}}}),
        json!({"text": "w", "fusion": {"max": {}}}),
    ] {
        let (s, b) = query(&api, "late", &body).await;
        assert_eq!(s, StatusCode::OK, "{body}: {b}");
    }
    // A field no segment carries: the client's error, never an empty answer or a `500`.
    let (s, b) = query(
        &api,
        "late",
        &json!({"multi": {"field": "nowhere", "vectors": [[1.0, 0.0, 0.0]]}}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "unknown_field", "{b}");
    assert_eq!(b["error"]["retryable"], false, "{b}");
    // A zero query vector under cosine has no direction, as a dense one has none.
    let (s, b) = put(
        &api,
        "cos",
        &json!({"distance_metric": "cosine_distance", "documents": [
            {"id": "a", "vector": [1.0, 2.0], "vectors": {"late": [[1.0, 2.0]]}}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = query(
        &api,
        "cos",
        &json!({"multi": {"field": "late", "vectors": [[1.0, 0.0], [0.0, 0.0]]}}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}

#[tokio::test]
async fn a_named_fields_width_is_one_an_index() {
    // Across two batches, before any fold (no schema) and after one (a schema that records
    // `vector`'s width only), and within one batch.
    let store = MemoryStore::new();
    let api = api(&store);
    let row = |id: &str, w: usize| json!({"id": id, "vector": [1.0, 2.0], "vectors": {"late": [vec![0.5; w]]}});
    let write = |docs: Vec<Value>| json!({"durability": "durable", "documents": docs});
    // Without a schema: the index's first batch, unfolded.
    let (s, b) = put(&api, "docs", &write(vec![row("a", 3)])).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = put(&api, "docs", &write(vec![row("b", 4)])).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "no schema: {b}");
    assert_eq!(b["error"]["code"], "schema_conflict", "no schema: {b}");
    fold(&api).await;
    // With a schema, which records `vector`'s width only, against a row still held.
    let (s, b) = put(&api, "docs", &write(vec![row("c", 3)])).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = put(&api, "docs", &write(vec![row("b", 4)])).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "schema: {b}");
    assert_eq!(b["error"]["code"], "schema_conflict", "schema: {b}");
    fold(&api).await;
    // ⚠️ With every row folded, the door holds nothing to compare: a new width is accepted
    // (no request on the write path), and the query that meets both is refused, loudly.
    let (s, b) = put(&api, "docs", &write(vec![row("d", 4)])).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = query(
        &api,
        "docs",
        &json!({"multi": {"field": "late", "vectors": [[1.0, 0.0, 0.0]]}}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    // Within one batch, on a new index: its first row decides.
    let (s, b) = put(&api, "new", &write(vec![row("a", 2), row("b", 5)])).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    // And within one document.
    let (s, b) = put(
        &api,
        "new",
        &write(vec![json!({"id": "a", "vector": [1.0, 2.0],
            "vectors": {"late": [[1.0, 2.0], [1.0, 2.0, 3.0]]}})]),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}

#[tokio::test]
async fn a_multi_query_keeps_the_depth() {
    use pstore_testkit::depth::DepthCounting;
    // One field read a segment, in the leg round beside the other legs': D-34's four rounds,
    // however many segments. Each query on a cold server, so nothing is cached.
    //
    // ⚠️ A field wider than the open round's suffix read (12.8 KB a segment, against 8 KiB),
    // so the leg's read is a request of its own: a field the open round happened to read
    // whole would cost no round wherever it was read.
    let wide = |i: usize| -> Vec<Vec<f32>> {
        (0..5)
            .map(|j| {
                (0..64)
                    .map(|d| (((i * 5 + j) * 64 + d) as f32 * 0.01).sin())
                    .collect()
            })
            .collect()
    };
    // ⚠️ At one segment as well as four: the meter counts a round when a request starts with
    // nothing in flight, so a chain inside one segment's task hides behind another
    // segment's reads overlapping it. Alone, every chain shows.
    let q: Vec<Vec<f32>> = [wide(100), wide(101)].concat();
    let multi = json!({"field": "late", "vectors": q});
    for segments in [1, 4] {
        let store = DepthCounting::new(MemoryStore::new());
        let cold = || Api::new(Accounted::new(store.clone()), LaneId(1)).unwrap();
        let writer = cold();
        for k in 0..segments {
            let docs: Vec<Value> = (k * 10..k * 10 + 10)
                .map(|i| doc(&format!("d{i:02}"), i as i64, &wide(i)))
                .collect();
            let body = json!({"durability": "durable", "documents": docs});
            let (s, b) = send(&writer, "PUT", "/v1/indexes/docs/documents", Some(&body)).await;
            assert_eq!(s, StatusCode::OK, "{b}");
            let (s, b) = send(&writer, "POST", "/v1/admin/fold", None).await;
            assert_eq!(s, StatusCode::OK, "{b}");
        }
        let mut depths = Vec::new();
        for body in [
            json!({"vector": [0.3, 0.9, -0.2, 1.0], "top_k": 10}),
            json!({"multi": multi, "top_k": 10}),
            json!({"vector": [0.3, 0.9, -0.2, 1.0], "multi": multi, "text": "word1",
                   "top_k": 10}),
        ] {
            let api = cold();
            store.reset();
            let (status, b) = send(&api, "POST", "/v1/indexes/docs/query", Some(&body)).await;
            assert_eq!(status, StatusCode::OK, "{b}");
            assert_eq!(ids(&b).len(), 10, "{body}: {b}");
            depths.push(store.depth());
        }
        assert_eq!(
            depths,
            [4, 4, 4],
            "{segments} segment(s): dense, multi, hybrid"
        );
    }
}
