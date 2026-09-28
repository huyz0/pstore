//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Token predicates (M14.2): `ContainsAllTokens`, `ContainsAnyToken` and
//! `ContainsTokenSequence`, analyzed by the index's analyzer wherever a filter is evaluated.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_format::text::{Analyzer, analyze};
use pstore_server::Api;
use pstore_testkit::depth::DepthCounting;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use tower::ServiceExt;

type Store = DepthCounting<MemoryStore>;
type A = Arc<Api<Store>>;

fn world(lanes: &[u64]) -> (Store, Vec<A>) {
    let depth = DepthCounting::new(MemoryStore::new());
    let store = Accounted::new(depth.clone());
    let apis = lanes
        .iter()
        .map(|l| Api::new(store.clone(), LaneId(*l)).unwrap())
        .collect();
    (depth, apis)
}

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "45")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn write(api: &A, mut body: Value) {
    body["durability"] = json!("durable");
    let (s, b) = send(api, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn query(api: &A, q: &Value) -> Value {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", q).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b
}

fn ids_of(b: &Value) -> BTreeSet<String> {
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

const STEM: Analyzer = Analyzer {
    language: pstore_format::text::Language::English,
    stemming: true,
    remove_stopwords: false,
    case_sensitive: false,
    ascii_folding: false,
};

const VOCAB: &[&str] = &[
    "run", "runs", "running", "ran", "dog", "dogs", "quick", "the", "fox", "foxes", "jump",
    "jumping", "jumped", "Brown",
];

/// Row `i`: its text, its title, and an array of strings.
fn row(i: usize) -> (String, String, String) {
    let mut x = (i as u64)
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let mut word = || {
        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        VOCAB[(x >> 33) as usize % VOCAB.len()]
    };
    let text: Vec<&str> = (0..3 + i % 4).map(|_| word()).collect();
    let title: Vec<&str> = (0..2).map(|_| word()).collect();
    (text.join(" "), title.join(" "), word().to_owned())
}

fn doc(i: usize) -> Value {
    let (text, title, tag) = row(i);
    json!({"id": format!("d{i:05}"), "vector": [1.0, 0.5], "text": text,
           "attributes": {"title": title, "tags": [tag], "n": i}})
}

/// The declaration every index here is created with.
fn stemming() -> Value {
    json!({"text": {"type": "string", "full_text_search": {"stemming": true}}})
}

async fn seeded(a: &A, rows: std::ops::Range<usize>, folded: bool) {
    for chunk in rows.collect::<Vec<_>>().chunks(500) {
        let docs: Vec<Value> = chunk.iter().map(|i| doc(*i)).collect();
        write(a, json!({"schema": stemming(), "documents": docs})).await;
        if folded {
            fold(a).await;
        }
    }
}

/// `op`'s truth for a row's value of `attr` under `an`, as M14.2 defines it.
fn holds(an: &Analyzer, op: &str, query: &str, value: Option<&str>) -> bool {
    let Some(v) = value else { return false };
    let (q, r) = (analyze(an, query), analyze(an, v));
    match op {
        "ContainsAllTokens" => q.iter().all(|t| r.contains(t)),
        "ContainsAnyToken" => q.iter().any(|t| r.contains(t)),
        _ => q.is_empty() || r.windows(q.len()).any(|w| w == q.as_slice()),
    }
}

/// Every filter the tests run, with its brute-force answer over `rows` under `an`.
fn cases(an: &Analyzer, rows: std::ops::Range<usize>) -> Vec<(Value, BTreeSet<String>)> {
    let mut out = Vec::new();
    for op in [
        "ContainsAllTokens",
        "ContainsAnyToken",
        "ContainsTokenSequence",
    ] {
        for text in [
            "running dogs",
            "runs",
            "quick running",
            "jumped fox",
            "brown",
            "",
        ] {
            for attr in ["text", "title", "tags", "missing"] {
                let leaf = json!([attr, op, text]);
                let admitted = |i: usize| {
                    let (t, ti, _) = row(i);
                    let v = match attr {
                        "text" => Some(t),
                        "title" => Some(ti),
                        _ => None,
                    };
                    holds(an, op, text, v.as_deref())
                };
                for (wrap, f) in [
                    ("plain", leaf.clone()),
                    ("not", json!(["Not", leaf.clone()])),
                    ("and", json!(["And", [leaf.clone(), ["n", "Lt", 1000]]])),
                    ("or", json!(["Or", [leaf.clone(), ["n", "Lt", 100]]])),
                ] {
                    let want = rows
                        .clone()
                        .filter(|i| match wrap {
                            "plain" => admitted(*i),
                            "not" => !admitted(*i),
                            "and" => admitted(*i) && *i < 1000,
                            _ => admitted(*i) || *i < 100,
                        })
                        .map(|i| format!("d{i:05}"))
                        .collect();
                    out.push((f, want));
                }
            }
        }
    }
    out
}

/// Every path a filter is evaluated on gives the brute-force answer.
async fn every_path_agrees(a: &A, rows: std::ops::Range<usize>) {
    for (f, want) in cases(&STEM, rows.clone()) {
        let ordered = json!({"rank_by": ["id", "asc"], "top_k": 10_000, "filters": f});
        assert_eq!(ids_of(&query(a, &ordered).await), want, "ordered {f}");
        let ranked = json!({"vector": [1.0, 0.5], "top_k": 10_000, "filters": f});
        assert_eq!(ids_of(&query(a, &ranked).await), want, "ranked {f}");
        let agg = json!({"aggregate_by": {"n": ["Count", "id"]}, "filters": f});
        let b = query(a, &agg).await;
        assert_eq!(
            b["aggregations"]["n"].as_u64().unwrap(),
            want.len() as u64,
            "aggregated {f}: {b}"
        );
    }
}

#[tokio::test]
async fn each_token_predicate_equals_brute_force_under_the_index_analyzer() {
    // The stemmed answer must differ from the default's, or binding would be untested.
    let differs = cases(&STEM, 0..2000)
        .iter()
        .zip(cases(&Analyzer::default(), 0..2000))
        .any(|((_, s), (_, d))| *s != d);
    assert!(differs, "the corpus cannot tell the analyzers apart");
    let (_, w) = world(&[1]);
    let a = &w[0];
    seeded(a, 0..2000, true).await;
    every_path_agrees(a, 0..2000).await;
}

#[tokio::test]
async fn before_the_first_fold_the_declared_analyzer_applies() {
    let (_, w) = world(&[1]);
    let a = &w[0];
    seeded(a, 0..300, false).await;
    every_path_agrees(a, 0..300).await;
}

#[tokio::test]
async fn a_token_predicate_decides_at_the_fold_as_a_query_does() {
    let (_, w) = world(&[1]);
    let a = &w[0];
    let run = json!(["text", "ContainsAnyToken", "running"]);
    // In the fold that creates the index: its analyzer is the one this fold records.
    write(
        a,
        json!({"schema": stemming(), "documents": [
            {"id": "r", "vector": [1.0, 0.5], "text": "he runs daily"},
            {"id": "s", "vector": [1.0, 0.5], "text": "she sings"},
            {"id": "t", "vector": [1.0, 0.5], "text": "they ran"}]}),
    )
    .await;
    write(a, json!({"delete_by_filter": run})).await;
    fold(a).await;
    let all = json!({"rank_by": ["id", "asc"], "top_k": 100, "include_attributes": true});
    assert_eq!(
        ids_of(&query(a, &all).await),
        BTreeSet::from(["s".into(), "t".into()])
    );
    // `Not` of a token predicate patches exactly the complement.
    write(
        a,
        json!({"documents": [{"id": "u", "vector": [1.0, 0.5], "text": "we run"}]}),
    )
    .await;
    fold(a).await;
    write(
        a,
        json!({"patch_by_filter": {"filters": ["Not", run], "attributes": {"m": 1}}}),
    )
    .await;
    fold(a).await;
    let b = query(a, &all).await;
    let marked: BTreeSet<String> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["attributes"]["m"] == 1)
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(marked, BTreeSet::from(["s".into(), "t".into()]), "{b}");
    // A conditional upsert, judged by the stemmed text.
    write(
        a,
        json!({"documents": [{"id": "u", "vector": [1.0, 0.5], "text": "replaced"}],
               "upsert_condition": run}),
    )
    .await;
    write(
        a,
        json!({"documents": [{"id": "s", "vector": [1.0, 0.5], "text": "replaced"}],
               "upsert_condition": run}),
    )
    .await;
    fold(a).await;
    let b = query(
        a,
        &json!({"rank_by": ["id", "asc"], "top_k": 100,
        "filters": ["text", "ContainsAnyToken", "replaced"]}),
    )
    .await;
    assert_eq!(ids_of(&b), BTreeSet::from(["u".into()]), "{b}");
}

#[tokio::test]
async fn a_token_predicate_adds_no_round() {
    let (depth, w) = world(&[1]);
    let a = &w[0];
    seeded(a, 0..1500, true).await;
    let f = json!(["text", "ContainsTokenSequence", "quick running"]);
    for q in [
        json!({"rank_by": ["id", "asc"], "top_k": 50}),
        json!({"vector": [1.0, 0.5], "top_k": 50}),
    ] {
        depth.reset();
        query(a, &q).await;
        let unfiltered = depth.depth();
        let mut filtered = q.clone();
        filtered["filters"] = f.clone();
        depth.reset();
        query(a, &filtered).await;
        // M9b's rule: a filter adds no round trip. Measured 3 and 4 respectively: a ranked
        // query's depth is the vector path's, before M14 and with no filter.
        assert_eq!(depth.depth(), unfiltered, "{filtered}");
        if q.get("rank_by").is_some() {
            assert!(unfiltered <= 3, "{q}: depth {unfiltered}");
        }
    }
}

#[tokio::test]
async fn what_a_token_predicate_cannot_mean_is_refused() {
    let (_, w) = world(&[1]);
    let a = &w[0];
    seeded(a, 0..10, true).await;
    for f in [
        json!(["text", "ContainsAnyToken", 5]),
        json!(["text", "ContainsAllTokens", ["a"]]),
        json!(["text", "ContainsTokenSequence", null]),
    ] {
        let (s, b) = send(
            a,
            "POST",
            "/v1/indexes/docs/query",
            &json!({"rank_by": ["id", "asc"], "filters": f}),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{f}: {b}");
        assert!(
            b["error"]["message"].as_str().unwrap().contains("string"),
            "{f}: {b}"
        );
    }
}
