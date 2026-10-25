//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M63: the query's terms marked in each result's text.
//!
//! Every expected string is written out by hand from the spec's rules, never computed by the
//! code under test.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

async fn send<S: BlobStore + Clone + 'static>(
    api: &Arc<Api<S>>,
    method: &str,
    uri: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "31")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn api() -> Arc<Api<MemoryStore>> {
    Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap()
}

/// Writes `rows` as `(id, text)` durably, declaring `fts` when given.
async fn put<S: BlobStore + Clone + 'static>(
    api: &Arc<Api<S>>,
    rows: &[(&str, &str)],
    fts: Option<Value>,
) {
    let docs: Vec<Value> = rows
        .iter()
        .map(|(id, t)| json!({"id": id, "vector": [1.0, 0.5], "text": t}))
        .collect();
    let mut body = json!({"durability": "durable", "documents": docs});
    if let Some(f) = fts {
        body["schema"] = json!({"text": {"type": "string", "full_text_search": f}});
    }
    let (s, b) = send(api, "PUT", "/v1/indexes/docs/documents", Some(&body)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>) -> u64 {
    let (s, b) = send(api, "POST", "/v1/admin/fold", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["epoch"].as_u64().unwrap()
}

async fn query<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>, q: &Value) -> Value {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", Some(q)).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b
}

/// The one result's `$highlight`.
async fn one<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>, q: &Value) -> String {
    let b = query(api, q).await;
    let r = b["results"].as_array().unwrap();
    assert_eq!(r.len(), 1, "{b}");
    r[0]["$highlight"]
        .as_str()
        .unwrap_or_else(|| panic!("no $highlight: {b}"))
        .to_owned()
}

#[tokio::test]
async fn highlight_marks_the_querys_terms() {
    // The default analyzer: lowercase, no stemming. `Running` is `running`, not `run`.
    let a = api();
    put(&a, &[("r", "Running RUN run!")], None).await;
    let q = |text: Value, h: Value| json!({"text": text, "top_k": 5, "highlight": h});
    assert_eq!(
        one(&a, &q(json!("run"), json!({}))).await,
        "Running <em>RUN</em> <em>run</em>!"
    );
    assert_eq!(
        one(&a, &q(json!("run"), json!({"pre": "[", "post": "]"}))).await,
        "Running [RUN] [run]!"
    );
    assert_eq!(
        one(&a, &q(json!("run"), json!({"pre": "", "post": ""}))).await,
        "Running RUN run!"
    );
    // Two text legs: both legs' terms are marked.
    let b = api();
    put(&b, &[("r", "run fast slow")], None).await;
    assert_eq!(
        one(&b, &q(json!(["run", "fast"]), json!({}))).await,
        "<em>run</em> <em>fast</em> slow"
    );
    // A query of stopwords alone analyzes to nothing under a stopword analyzer: nothing is
    // marked, and the query is not refused.
    let c = api();
    put(
        &c,
        &[("r", "the cat")],
        Some(json!({"remove_stopwords": true, "language": "english"})),
    )
    .await;
    let r = query(&c, &json!({"text": "the cat", "top_k": 5, "highlight": {}})).await;
    assert_eq!(r["results"][0]["$highlight"], "the <em>cat</em>", "{r}");
    // A text leg that analyzes to no term marks nothing, and is not refused: the dense leg
    // still returns the row.
    let r = query(
        &c,
        &json!({"vector": [1.0, 0.5], "text": "the", "top_k": 5, "highlight": {}}),
    )
    .await;
    assert_eq!(r["results"][0]["$highlight"], "the cat", "{r}");
}

#[tokio::test]
async fn highlight_follows_the_indexs_analyzer() {
    // English, stemming, stopwords, folding: `run` marks `Running` and `runs`, not `ran`
    // (Snowball leaves `ran`); `The` never; and `café`, in either spelling, for `cafe`.
    let a = api();
    let text = "The Running runs ran caf\u{e9} cafe\u{301}.";
    put(
        &a,
        &[("r", text)],
        Some(json!({"language": "english", "stemming": true,
                    "remove_stopwords": true, "ascii_folding": true})),
    )
    .await;
    for when in ["unfolded", "folded"] {
        assert_eq!(
            one(&a, &json!({"text": "run", "top_k": 5, "highlight": {}})).await,
            "The <em>Running</em> <em>runs</em> ran caf\u{e9} cafe\u{301}.",
            "{when}"
        );
        assert_eq!(
            one(&a, &json!({"text": "cafe", "top_k": 5, "highlight": {}})).await,
            "The Running runs ran <em>caf\u{e9}</em> <em>cafe\u{301}</em>.",
            "{when}: the accent stays inside the marks"
        );
        // The query is analyzed by the index's analyzer too: `running` is the term `run`, and
        // `Café` the term `cafe`. Under the default analyzer neither would match the text.
        assert_eq!(
            one(&a, &json!({"text": "running", "top_k": 5, "highlight": {}})).await,
            "The <em>Running</em> <em>runs</em> ran caf\u{e9} cafe\u{301}.",
            "{when}: a query word that stems"
        );
        assert_eq!(
            one(
                &a,
                &json!({"text": "Caf\u{e9}", "top_k": 5, "highlight": {}})
            )
            .await,
            "The Running runs ran <em>caf\u{e9}</em> <em>cafe\u{301}</em>.",
            "{when}: a query word that folds"
        );
        // A stopword is never marked, even when asked for.
        let r = query(
            &a,
            &json!({"text": "the cafe", "top_k": 5, "highlight": {}}),
        )
        .await;
        assert!(
            !r["results"][0]["$highlight"]
                .as_str()
                .unwrap()
                .contains("<em>The"),
            "{when}: {r}"
        );
        if when == "unfolded" {
            fold(&a).await;
        }
    }
}

#[tokio::test]
async fn highlight_fragments_hold_the_most_marks() {
    // A dense leg too, so the row is returned whether or not the text matches.
    let hl = |q: &str, max: u32| json!({"vector": [1.0, 0.5], "text": q, "top_k": 5, "highlight": {"max_chars": max}});
    // Whole when it fits: exactly max_chars characters.
    let a = api();
    put(&a, &[("r", "aa bb cc")], None).await;
    assert_eq!(one(&a, &hl("bb", 8)).await, "aa <em>bb</em> cc");
    // Three two-character tokens fit in 10 characters. The first window holding `gg`.
    let a = api();
    put(&a, &[("r", "aa bb cc dd ee ff gg hh")], None).await;
    assert_eq!(one(&a, &hl("gg", 10)).await, "…ee ff <em>gg</em>…");
    // The densest window over an earlier, sparser one.
    let a = api();
    put(&a, &[("r", "aa bb xx yy zz bb bb")], None).await;
    assert_eq!(one(&a, &hl("bb", 8)).await, "…zz <em>bb</em> <em>bb</em>");
    // The earliest among equals.
    let a = api();
    put(&a, &[("r", "aa bb xx yy bb zz")], None).await;
    assert_eq!(one(&a, &hl("bb", 8)).await, "aa <em>bb</em> xx…");
    // Nothing marked: from the start.
    let a = api();
    put(&a, &[("r", "aa bb cc dd ee")], None).await;
    assert_eq!(one(&a, &hl("zzz", 8)).await, "aa bb cc…");
    // A token longer than max_chars, cut at max_chars characters and followed by an ellipsis,
    // marked when it matches.
    let a = api();
    put(&a, &[("r", "abcdefghijklmnop rest")], None).await;
    assert_eq!(one(&a, &hl("abcdefghijklmnop", 5)).await, "<em>abcde</em>…");
    let a = api();
    put(&a, &[("r", "abcdefghij kl")], None).await;
    assert_eq!(one(&a, &hl("zzz", 5)).await, "abcde…");
    // A long marked token outscores an earlier window that holds no mark (mutation sweep:
    // the long-token test as `==`).
    let a = api();
    put(&a, &[("r", "aa abcdefghij")], None).await;
    assert_eq!(one(&a, &hl("abcdefghij", 5)).await, "…<em>abcde</em>…");
    // A token of exactly max_chars characters is not "longer": it fits, whole, with nothing
    // after it and so no trailing ellipsis (mutation sweep: `>` as `>=`).
    let a = api();
    put(&a, &[("r", "fg abcde")], None).await;
    assert_eq!(one(&a, &hl("abcde", 5)).await, "…<em>abcde</em>");
    // Characters, not bytes: 日本語 is nine bytes and three characters.
    let a = api();
    put(&a, &[("r", "日本語 äöü xyz")], None).await;
    assert_eq!(one(&a, &hl("xyz", 7)).await, "…äöü <em>xyz</em>");
    // A text with no token longer than max_chars: its first max_chars characters.
    let a = api();
    put(&a, &[("r", "...---...---")], None).await;
    // (the text has no token to match, so the query must match it another way)
    let r = query(
        &a,
        &json!({"vector": [1.0, 0.5], "text": "zzz", "top_k": 5, "highlight": {"max_chars": 5}}),
    )
    .await;
    assert_eq!(r["results"][0]["$highlight"], "...--…", "{r}");
}

#[tokio::test]
async fn highlight_rides_every_ranked_answer() {
    let a = api();
    put(&a, &[("r", "run far"), ("s", "run near")], None).await;
    // A row with no text attribute, and one whose text attribute is a number.
    let (s, b) = send(
        &a,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": [
            {"id": "n", "vector": [1.0, 0.5], "attributes": {"label": 7, "other": "run text"}}]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let q = json!({"vector": [1.0, 0.5], "text": "run", "top_k": 5,
                   "highlight": {"attr": "label"}});
    let b = query(&a, &q).await;
    assert!(
        b["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r.get("$highlight").is_none()),
        "no string attribute `label` anywhere: {b}"
    );
    // Another string attribute is named by `attr`, and analyzed by the text analyzer.
    let q = json!({"vector": [1.0, 0.5], "text": "run", "top_k": 5,
                   "highlight": {"attr": "other"}});
    let b = query(&a, &q).await;
    let n = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "n")
        .unwrap();
    assert_eq!(n["$highlight"], "<em>run</em> text", "{b}");
    // `include_attributes` false and `exclude_attributes` of the text do not touch it.
    for extra in [
        json!({"include_attributes": false}),
        json!({"exclude_attributes": ["text"]}),
    ] {
        let mut q = json!({"text": "run", "top_k": 5, "highlight": {}});
        for (k, v) in extra.as_object().unwrap() {
            q[k] = v.clone();
        }
        let b = query(&a, &q).await;
        for r in b["results"].as_array().unwrap() {
            let h = r["$highlight"].as_str().unwrap();
            assert!(h.starts_with("<em>run</em> "), "{extra}: {b}");
            assert!(
                r.get("attributes").is_none_or(|a| a.get("text").is_none()),
                "{b}"
            );
        }
    }
    // On a boosted query's final rows only, in the boosted order.
    let q = json!({"text": "run", "top_k": 1, "highlight": {},
                   "boost": {"window": 2, "terms": [
                       {"attr": "other", "fn": "eq", "weight": 5.0, "value": "x"}]}});
    let b = query(&a, &q).await;
    assert_eq!(b["results"].as_array().unwrap().len(), 1, "{b}");
    assert!(b["results"][0]["$highlight"].is_string(), "{b}");
    // Per sub-query in a multi-query; one without it has none.
    let multi = json!({"queries": [
        {"text": "run", "top_k": 5, "highlight": {}},
        {"text": "run", "top_k": 5},
        {"text": "far", "top_k": 5, "highlight": {"pre": "(", "post": ")"}}]});
    let b = query(&a, &multi).await;
    let subs = b["results"].as_array().unwrap();
    assert!(
        subs[0][0]["$highlight"]
            .as_str()
            .unwrap()
            .contains("<em>run</em>"),
        "{b}"
    );
    assert!(subs[1][0].get("$highlight").is_none(), "{b}");
    assert_eq!(subs[2][0]["$highlight"], "run (far)", "{b}");
}

#[tokio::test]
async fn highlight_uses_the_epochs_analyzer() {
    // Dropped and re-created under another analyzer: `as_of` the old epoch marks by the old
    // one, which stems.
    let a = api();
    put(
        &a,
        &[("r", "he runs daily")],
        Some(json!({"stemming": true})),
    )
    .await;
    let past = fold(&a).await;
    let (s, b) = send(&a, "DELETE", "/v1/indexes/docs", Some(&json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    put(
        &a,
        &[("r", "he runs daily")],
        Some(json!({"case_sensitive": true})),
    )
    .await;
    fold(&a).await;
    let q = json!({"text": "running", "top_k": 5, "highlight": {}, "as_of": past});
    assert_eq!(one(&a, &q).await, "he <em>runs</em> daily");
    // The present index does not stem: `running` matches nothing.
    let r = query(&a, &json!({"text": "running", "top_k": 5, "highlight": {}})).await;
    assert!(r["results"].as_array().unwrap().is_empty(), "{r}");
}

#[tokio::test]
async fn highlights_are_refused_at_the_door() {
    let a = api();
    put(&a, &[("r", "run")], None).await;
    let q = |h: Value| json!({"text": "run", "top_k": 5, "highlight": h});
    let cases = [
        ("not an object", q(json!("<em>"))),
        ("an unknown field", q(json!({"color": "red"}))),
        ("an empty attr", q(json!({"attr": ""}))),
        ("id", q(json!({"attr": "id"}))),
        ("a $ attr", q(json!({"attr": "$dist"}))),
        ("a long pre", q(json!({"pre": "x".repeat(17)}))),
        ("a long post", q(json!({"post": "x".repeat(17)}))),
        ("a number pre", q(json!({"pre": 1}))),
        ("max_chars 0", q(json!({"max_chars": 0}))),
        ("max_chars 2001", q(json!({"max_chars": 2001}))),
        ("a fractional max_chars", q(json!({"max_chars": 200.0}))),
        ("a string max_chars", q(json!({"max_chars": "200"}))),
        (
            "no text leg",
            json!({"vector": [1.0, 0.5], "top_k": 5, "highlight": {}}),
        ),
        (
            "beside rank_by",
            json!({"rank_by": ["n", "asc"], "top_k": 5, "highlight": {}}),
        ),
        (
            "beside aggregate_by",
            json!({"aggregate_by": {"c": ["Count", "id"]}, "highlight": {}}),
        ),
    ];
    for (what, body) in &cases {
        let (s, b) = send(&a, "POST", "/v1/indexes/docs/query", Some(body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{what}: {b}");
        assert!(
            b["error"]["message"]
                .as_str()
                .unwrap()
                .contains("highlight"),
            "{what}: the message names highlight: {b}"
        );
    }
    // The limits themselves, and `null`: no highlight.
    for h in [
        json!({"max_chars": 1}),
        json!({"max_chars": 2000}),
        json!({"pre": "x".repeat(16), "post": ""}),
        Value::Null,
    ] {
        let r = query(&a, &q(h.clone())).await;
        assert_eq!(r["results"].as_array().unwrap().len(), 1, "{h}");
    }
}

#[tokio::test]
async fn a_highlight_costs_nothing() {
    use pstore_testkit::depth::DepthCounting;
    for segments in [1, 4] {
        let store = DepthCounting::new(MemoryStore::new());
        let cold = || Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
        let writer = cold();
        for k in 0..segments {
            let rows: Vec<(String, String)> = (0..10)
                .map(|i| (format!("d{k}{i}"), format!("common word{i} run tail{k}")))
                .collect();
            let refs: Vec<(&str, &str)> =
                rows.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            put(&writer, &refs, None).await;
            fold(&writer).await;
        }
        let mut seen = Vec::new();
        for body in [
            json!({"text": "common run", "top_k": 20}),
            json!({"text": "common run", "top_k": 20, "highlight": {}}),
        ] {
            let api = cold();
            store.reset();
            let b = query(&api, &body).await;
            assert_eq!(
                b["results"].as_array().unwrap().len(),
                20.min(segments * 10)
            );
            let cost = &b["meta"]["cost"];
            seen.push((
                store.depth(),
                cost["blob_reads"].clone(),
                cost["bytes_read"].clone(),
            ));
        }
        assert_eq!(seen[0].0, 4, "{segments} segment(s): {seen:?}");
        assert_eq!(
            seen[0], seen[1],
            "{segments} segment(s): plain, then highlighted"
        );
    }
}

#[test]
fn the_docs_say_marks_are_not_escaped() {
    // A predicate over files (gate-design): the sentence that makes escaping the client's.
    let docs = include_str!("../../../docs/deploy.md");
    let at = docs
        .find("## Highlighting")
        .expect("a Highlighting section");
    let section = &docs[at..];
    let section = &section[..section.find("\n## ").unwrap_or(section.len())];
    assert!(section.contains("not escaped"), "{section}");
}
