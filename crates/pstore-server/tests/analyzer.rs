//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Full-text analyzer options (M14.1): declared on a write, fixed in the index's schema, and
//! used wherever text becomes terms.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

fn apis(lanes: &[u64]) -> (Accounted<MemoryStore>, Vec<A>) {
    let store = Accounted::new(MemoryStore::new());
    let apis = lanes
        .iter()
        .map(|l| Api::new(store.clone(), LaneId(*l)).unwrap())
        .collect();
    (store, apis)
}

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "44")
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

/// A durable write of `texts` as `(id, text)`, declaring `fts` when given.
async fn put(api: &A, texts: &[(&str, &str)], fts: Option<Value>) -> (StatusCode, Value) {
    let docs: Vec<Value> = texts
        .iter()
        .map(|(id, t)| json!({"id": id, "vector": [1.0, 0.5], "text": t}))
        .collect();
    let mut body = json!({"durability": "durable", "documents": docs});
    if let Some(f) = fts {
        body["schema"] = json!({"text": {"type": "string", "full_text_search": f}});
    }
    send(api, "PUT", "/v1/indexes/docs/documents", &body).await
}

async fn ok_put(api: &A, texts: &[(&str, &str)], fts: Option<Value>) {
    let (s, b) = put(api, texts, fts).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// A BM25 query's `(id, raw score)`, best first.
async fn bm25(api: &A, text: &str) -> Vec<(String, f64)> {
    let (s, b) = send(
        api,
        "POST",
        "/v1/indexes/docs/query",
        // Raw BM25: the leg alone under `sum`, weight 1 (M9g.2), not a fused rank.
        &json!({"text": text, "top_k": 100, "fusion": {"sum": {}}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{text}: {b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                r["score"].as_f64().unwrap(),
            )
        })
        .collect()
}

async fn ids(api: &A, text: &str) -> Vec<String> {
    bm25(api, text)
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

async fn stats(api: &A) -> Value {
    let (s, b) = send(api, "GET", "/v1/indexes/docs", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

const CORPUS: &[(&str, &str)] = &[
    ("a", "quarterly revenue report for the board"),
    ("b", "revenue by quarter and region"),
    ("c", "the quarterly revenue call, revenue up"),
    ("d", "board meeting minutes"),
    (
        "e",
        "a long report on regional revenue revenue revenue trends and outlook",
    ),
];

#[tokio::test]
async fn an_undeclared_index_answers_as_before_m14() {
    let (_, w) = apis(&[1]);
    let a = &w[0];
    ok_put(a, CORPUS, None).await;
    fold(a).await;
    // Measured on the tree before M14 (commit c624be4), the same corpus and queries.
    let got = bm25(a, "quarterly revenue").await;
    let want = PRE_M14_QUARTERLY_REVENUE;
    assert_eq!(got.len(), want.len(), "{got:?}");
    for ((id, score), (wid, wscore)) in got.iter().zip(want) {
        assert_eq!(id, wid, "{got:?}");
        assert!((score - wscore).abs() < 1e-6, "{id}: {score} vs {wscore}");
    }
    assert!(stats(a).await["schema"]["full_text_search"]["stemming"] == false);
}

/// `(id, score)` for `quarterly revenue` over [`CORPUS`], measured before M14.
const PRE_M14_QUARTERLY_REVENUE: &[(&str, f64)] = &[
    ("c", 1.286361),
    ("a", 1.1787057),
    ("e", 0.38774547),
    ("b", 0.31241912),
];

/// A declaration, its documents, a query, what it finds, and what the default finds.
type Case = (
    Value,
    Vec<(&'static str, &'static str)>,
    &'static str,
    Vec<&'static str>,
    Vec<&'static str>,
);

#[tokio::test]
async fn each_option_applies_before_and_after_the_fold() {
    // (declaration, the documents, the query, what it must find, what the default finds)
    let cases: Vec<Case> = vec![
        (
            json!({"stemming": true}),
            vec![("r", "he runs daily"), ("n", "nothing here")],
            "running",
            vec!["r"],
            vec![],
        ),
        (
            json!({"remove_stopwords": true}),
            vec![("t", "the cat"), ("n", "nothing here")],
            "the",
            vec![],
            vec!["t"],
        ),
        (
            json!({"case_sensitive": true}),
            vec![("l", "apple pie"), ("u", "Apple pie")],
            "Apple",
            vec!["u"],
            vec!["l", "u"],
        ),
        (
            json!({"ascii_folding": true}),
            vec![("c", "café au lait"), ("n", "nothing here")],
            "cafe",
            vec!["c"],
            vec![],
        ),
        (
            json!({"language": "german", "stemming": true}),
            vec![("h", "die alten Häuser"), ("n", "nichts hier")],
            "Haus",
            vec!["h"],
            vec![],
        ),
    ];
    for (fts, docs, query, want, default_finds) in cases {
        for declared in [Some(fts.clone()), None] {
            let (_, w) = apis(&[1]);
            let a = &w[0];
            ok_put(a, &docs, declared.clone()).await;
            let expect = if declared.is_some() {
                &want
            } else {
                &default_finds
            };
            let mut before = ids(a, query).await;
            before.sort();
            assert_eq!(
                &before, expect,
                "{fts} declared={declared:?}, before the fold"
            );
            fold(a).await;
            let mut after = ids(a, query).await;
            after.sort();
            assert_eq!(
                &after, expect,
                "{fts} declared={declared:?}, after the fold"
            );
        }
    }
}

/// BM25 as D-30 defines it, over whitespace-separated lowercase text with no stemming.
fn oracle(corpus: &[(&str, &str)], query: &str, k1: f64, b: f64) -> BTreeMap<String, f64> {
    let docs: Vec<(String, Vec<String>)> = corpus
        .iter()
        .map(|(id, t)| {
            let toks = t
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
                .map(str::to_lowercase)
                .collect();
            ((*id).to_owned(), toks)
        })
        .collect();
    let n = docs.len() as f64;
    let avgdl = docs.iter().map(|(_, t)| t.len() as f64).sum::<f64>() / n;
    let mut terms: Vec<String> = query.split(' ').map(str::to_lowercase).collect();
    terms.dedup();
    let mut out = BTreeMap::new();
    for (id, toks) in &docs {
        let mut s = 0.0;
        for term in &terms {
            let df = docs.iter().filter(|(_, t)| t.contains(term)).count() as f64;
            let tf = toks.iter().filter(|t| *t == term).count() as f64;
            if tf == 0.0 {
                continue;
            }
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            let norm = k1 * (1.0 - b + b * toks.len() as f64 / avgdl);
            s += idf * tf * (k1 + 1.0) / (tf + norm);
        }
        if s > 0.0 {
            out.insert(id.clone(), s);
        }
    }
    out
}

#[tokio::test]
async fn k1_and_b_apply() {
    let (k1, b) = (2.5, 0.2);
    let (_, w) = apis(&[1]);
    let a = &w[0];
    ok_put(a, CORPUS, Some(json!({"k1": k1, "b": b}))).await;
    fold(a).await;
    let got: BTreeMap<String, f64> = bm25(a, "quarterly revenue").await.into_iter().collect();
    let want = oracle(CORPUS, "quarterly revenue", k1, b);
    let defaults = oracle(CORPUS, "quarterly revenue", 1.2, 0.75);
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>()
    );
    let mut differs = false;
    for (id, s) in &got {
        assert!((s - want[id]).abs() < 1e-4, "{id}: {s} vs {}", want[id]);
        differs |= (s - defaults[id]).abs() > 1e-3;
    }
    assert!(differs, "the declared k1 and b score as the defaults do");
    let st = stats(a).await;
    let f = &st["schema"]["full_text_search"];
    assert!((f["k1"].as_f64().unwrap() - k1).abs() < 1e-6, "{st}");
    assert!((f["b"].as_f64().unwrap() - b).abs() < 1e-6, "{st}");
}

#[tokio::test]
async fn an_analyzer_is_fixed_when_its_index_is_created() {
    let (_, w) = apis(&[1]);
    let a = &w[0];
    let stem = json!({"stemming": true});
    // Against unfolded rows: the door compares with this process's first declaration.
    ok_put(a, &[("r", "he runs daily")], Some(stem.clone())).await;
    let (s, b) = put(
        a,
        &[("x", "anything")],
        Some(json!({"case_sensitive": true})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    fold(a).await;
    // Against the schema.
    for other in [
        json!({"stemming": false}),
        json!(true),
        json!({"k1": 1.3, "stemming": true}),
    ] {
        let (s, b) = put(a, &[("x", "anything")], Some(other.clone())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{other}: {b}");
        assert_eq!(b["error"]["code"], "schema_conflict", "{other}: {b}");
        let msg = b["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("analyzer") && msg.contains("reindex") && msg.contains("copy"),
            "{msg}"
        );
    }
    // The same declaration, re-sent with its default k1 spelled out, is no conflict.
    ok_put(
        a,
        &[("y", "she ran")],
        Some(json!({"stemming": true, "k1": 1.2, "b": 0.75})),
    )
    .await;
    // Undeclared: accepted, and analyzed with the schema's analyzer.
    ok_put(a, &[("z", "they are running")], None).await;
    fold(a).await;
    let mut got = ids(a, "run").await;
    got.sort();
    assert_eq!(got, ["r", "z"]);
}

#[tokio::test]
async fn two_writers_creating_an_index_leave_one_analyzer() {
    let (_, w) = apis(&[1, 2]);
    let (a, b) = (&w[0], &w[1]);
    ok_put(
        a,
        &[("r", "he runs daily")],
        Some(json!({"stemming": true})),
    )
    .await;
    ok_put(
        b,
        &[("s", "she runs daily")],
        Some(json!({"case_sensitive": true})),
    )
    .await;
    let before = 0;
    fold(a).await;
    let st = stats(a).await;
    let f = &st["schema"]["full_text_search"];
    let stemmed = f["stemming"] == true;
    assert!(stemmed != (f["case_sensitive"] == true), "{st}");
    assert_eq!(st["rejected_rows"].as_u64().unwrap(), before + 1, "{st}");
    let survivor = if stemmed { "r" } else { "s" };
    assert_eq!(ids(a, "daily").await, [survivor]);
}

#[tokio::test]
async fn the_analyzer_is_recorded_and_never_shown_as_an_attribute() {
    let (store, w) = apis(&[1]);
    let a = &w[0];
    ok_put(
        a,
        &[("r", "he runs daily")],
        Some(json!({"stemming": true, "k1": 2.0})),
    )
    .await;
    fold(a).await;
    // A new process, reading HEAD cold.
    let fresh = Api::new(store.clone(), LaneId(9)).unwrap();
    assert_eq!(ids(&fresh, "running").await, ["r"]);
    let st = stats(&fresh).await;
    assert_eq!(st["schema"]["full_text_search"]["stemming"], true, "{st}");
    assert!((st["schema"]["full_text_search"]["k1"].as_f64().unwrap() - 2.0).abs() < 1e-6);
    let (s, b) = send(
        &fresh,
        "POST",
        "/v1/indexes/docs/query",
        &json!({"rank_by": ["id", "asc"], "include_attributes": true}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(!b.to_string().contains("$fts"), "{b}");
}

#[tokio::test]
async fn a_past_epoch_answers_with_its_own_analyzer() {
    let (_, w) = apis(&[1]);
    let a = &w[0];
    ok_put(
        a,
        &[("r", "he runs daily")],
        Some(json!({"stemming": true})),
    )
    .await;
    fold(a).await;
    let past = stats(a).await["updated_epoch"].clone();
    let (s, b) = send(a, "DELETE", "/v1/indexes/docs", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    ok_put(
        a,
        &[("r", "he runs daily")],
        Some(json!({"case_sensitive": true})),
    )
    .await;
    fold(a).await;
    assert!(ids(a, "running").await.is_empty(), "the new index stems");
    let q = json!({"text": "running", "top_k": 10, "as_of": past});
    let (s, b) = send(a, "POST", "/v1/indexes/docs/query", &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["results"][0]["id"], "r", "{b}");
}

#[tokio::test]
async fn what_an_analyzer_cannot_mean_is_refused() {
    let (_, w) = apis(&[1]);
    let a = &w[0];
    for (fts, why) in [
        (json!({"stemmer": true}), "stemmer"),
        (json!({"language": "klingon"}), "language"),
        (
            json!({"language": "french", "remove_stopwords": true}),
            "stopwords",
        ),
        (json!({"k1": 3.5}), "k1"),
        (json!({"k1": -0.1}), "k1"),
        (json!({"b": 1.5}), "b"),
        (json!({"tokenizer": "word_v9"}), "tokenizer"),
        (json!("yes"), "full_text_search"),
    ] {
        let (s, b) = put(a, &[("x", "anything")], Some(fts.clone())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{fts} was accepted: {b}");
        let msg = b["error"]["message"].as_str().unwrap();
        assert!(msg.contains(why), "{fts}: {msg:?} does not name {why:?}");
    }
    let body = json!({"durability": "durable",
        "schema": {"title": {"type": "string", "full_text_search": true}},
        "documents": [{"id": "x", "vector": [1.0, 0.5], "attributes": {"title": "t"}}]});
    let (s, b) = send(a, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("text field"),
        "{b}"
    );
}
