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
    let docs = include_str!("../../../../docs/deploy.md");
    let at = docs
        .find("## Highlighting")
        .expect("a Highlighting section");
    let section = &docs[at..];
    let section = &section[..section.find("\n## ").unwrap_or(section.len())];
    assert!(section.contains("not escaped"), "{section}");
}

// ---- M159: structured highlights. Every expected value is counted by hand. ----

/// The one result's `$highlights`, from a fresh index holding `docs`, queried by a dense leg
/// (so the row is found whatever the text) and a text leg `q`.
async fn structured_of(docs: Value, q: &str, h: Value) -> Value {
    let a = api();
    let (s, b) = send(
        &a,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": docs})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let b = query(
        &a,
        &json!({"vector": [1.0, 0.5], "text": q, "top_k": 5, "highlight": h}),
    )
    .await;
    let r = &b["results"][0];
    assert!(
        r.get("$highlight").is_none(),
        "no $highlight beside $highlights: {b}"
    );
    r.get("$highlights")
        .unwrap_or_else(|| panic!("no $highlights: {b}"))
        .clone()
}

/// [`structured_of`] over one row whose `text` is `text`.
async fn structured(text: &str, q: &str, h: Value) -> Value {
    structured_of(
        json!([{"id": "r", "vector": [1.0, 0.5], "text": text}]),
        q,
        h,
    )
    .await
}

/// `{"text": t}` for each of `ts`: matches with no offsets and no array index.
fn texts(ts: &[&str]) -> Value {
    Value::Array(ts.iter().map(|t| json!({"text": t})).collect())
}

#[tokio::test]
async fn structured_word_fragments_are_ranked_and_disjoint() {
    // Tokens 0..=14, one character each, one space apart: with max_chars 5 a window is three
    // tokens. Scores by starting token: 0:1 1:0 2:1 3:2 4:2 5:1 6:0 7:0 8:1 9:2 10:3 11:2
    // 12:1 13:0 14:0. Ranked: 10, then 3 (before 4, 9, 11, which overlap a taken window),
    // then 0. The best window comes after two weaker scored ones; the list is in rank order.
    let v = "q a b c q q d e f g q q q h i";
    let h = |limit: Value| json!({"max_chars": 5, "fragment_limit": limit});
    assert_eq!(
        structured(v, "q", h(json!(3))).await,
        texts(&[
            "<em>q</em> <em>q</em> <em>q</em>",
            "c <em>q</em> <em>q</em>",
            "<em>q</em> a b"
        ])
    );
    // The default limit is 1, and `fragment_by: "word"` alone is structured.
    assert_eq!(
        structured(v, "q", json!({"max_chars": 5, "fragment_by": "word"})).await,
        texts(&["<em>q</em> <em>q</em> <em>q</em>"])
    );
    // Limit 0: an empty list.
    assert_eq!(structured(v, "q", h(json!(0))).await, json!([]));
    // One scored window among disjoint ones: one match, though the limit is 3. Windows 0, 1
    // and 2 each hold the one `q`; 0 is the earliest and the others overlap it.
    assert_eq!(
        structured("a b q c d e f g", "q", h(json!(3))).await,
        texts(&["a b <em>q</em>"])
    );
    // Nothing marked: the first window alone.
    assert_eq!(
        structured("a b c d e f g", "q", h(json!(3))).await,
        texts(&["a b c"])
    );
}

#[tokio::test]
async fn structured_sentences_and_paragraphs_are_units() {
    let all = |by: &str| json!({"fragment_by": by, "fragment_limit": 16});
    // Units "x q." "q y!" "z q?" "w。" "q q": the last scores 2, three score 1 in text order,
    // and `w。` scores nothing. `。` splits with no whitespace after it.
    assert_eq!(
        structured("x q. q y! z q? w。q q", "q", all("sentence")).await,
        texts(&[
            "<em>q</em> <em>q</em>",
            "x <em>q</em>.",
            "<em>q</em> y!",
            "z <em>q</em>?"
        ])
    );
    // `3.5`: a terminator with no whitespace after it does not split.
    assert_eq!(
        structured("a 3.5 q. b", "q", all("sentence")).await,
        texts(&["a 3.5 <em>q</em>."])
    );
    // A line break ends a sentence.
    assert_eq!(
        structured("q a\nb q", "q", all("sentence")).await,
        texts(&["<em>q</em> a", "b <em>q</em>"])
    );
    // Paragraphs keep `. ` and split at `\n` and `\r\n`; every unit is trimmed.
    assert_eq!(
        structured("q. q\r\nq  \n  b", "q", all("paragraph")).await,
        texts(&["<em>q</em>. <em>q</em>", "<em>q</em>"])
    );
    // A whole value is trimmed too.
    assert_eq!(
        structured(" q a\n", "q", all("word")).await,
        texts(&["<em>q</em> a"])
    );
    // A break at the very start leaves no empty unit to be the earliest, unmarked fragment.
    assert_eq!(
        structured("\n\nq a", "zzz", all("paragraph")).await,
        texts(&["q a"])
    );
    // Units that touch, with no whitespace between them, do not overlap: each is taken,
    // whichever ranks first.
    assert_eq!(
        structured("q。q q", "q", all("sentence")).await,
        texts(&["<em>q</em> <em>q</em>", "<em>q</em>。"])
    );
    assert_eq!(
        structured("q q。q", "q", all("sentence")).await,
        texts(&["<em>q</em> <em>q</em>。", "<em>q</em>"])
    );
    // A unit longer than max_chars gives its best window, which ends at a token's end.
    assert_eq!(
        structured(
            "a b c q q. d",
            "q",
            json!({"fragment_by": "sentence", "fragment_limit": 16, "max_chars": 5})
        )
        .await,
        texts(&["c <em>q</em> <em>q</em>"])
    );
    // A long unit with no token: its first max_chars characters.
    assert_eq!(
        structured(
            "--------",
            "q",
            json!({"fragment_by": "word", "max_chars": 5})
        )
        .await,
        texts(&["-----"])
    );
    // `none`: the first max_chars characters, though the marks lie beyond them; whole when
    // they fit; a token the cut splits is marked over what is kept.
    let none = |n: u64| json!({"fragment_by": "none", "max_chars": n});
    assert_eq!(
        structured("a b c d q", "q", none(5)).await,
        texts(&["a b c"])
    );
    assert_eq!(
        structured("a b c d q", "q", none(9)).await,
        texts(&["a b c d <em>q</em>"])
    );
    assert_eq!(
        structured("a qqq b", "qqq", none(4)).await,
        texts(&["a <em>qq</em>"])
    );
}

#[tokio::test]
async fn structured_offsets_count_in_the_asked_unit() {
    // 😀 is 4 bytes, 2 UTF-16 units, 1 scalar; the accent U+0301 is 2, 1, 1; 中 is 3, 1, 1.
    // Bytes:  😀 0..4, `cafe` 5..9, accent 9..11, 中 12..15, q 16..17.
    // UTF-16: 😀 0..2, `cafe` 3..7, accent 7..8,  中 9..10,  q 11..12.
    // Scalars: 😀 0..1, `cafe` 2..6, accent 6..7, 中 8..9,   q 10..11.
    // The marked span holds the accent; the ranges are into the value, not into `text`.
    let v = "😀 cafe\u{301} 中 q";
    let text = "😀 <em>cafe\u{301}</em> 中 <em>q</em>";
    for (unit, fragment, marks) in [
        ("utf-8", json!([0, 17]), json!([[5, 11], [16, 17]])),
        ("utf-16", json!([0, 12]), json!([[3, 8], [11, 12]])),
        ("codepoints", json!([0, 11]), json!([[2, 7], [10, 11]])),
    ] {
        assert_eq!(
            structured(v, "cafe q", json!({"include_offsets": unit})).await,
            json!([{"text": text, "fragment_range": fragment, "match_ranges": marks}]),
            "{unit}"
        );
    }
    // No offsets without include_offsets.
    assert_eq!(
        structured(v, "cafe q", json!({"fragment_limit": 1})).await,
        texts(&[text])
    );
    // A trimmed unit's range starts after the whitespace it lost.
    assert_eq!(
        structured("\n q", "q", json!({"include_offsets": "utf-8"})).await,
        json!([{"text": "<em>q</em>", "fragment_range": [2, 3], "match_ranges": [[2, 3]]}])
    );
    // A token longer than max_chars is cut, and its range ends at the cut. No ellipsis.
    assert_eq!(
        structured(
            "a qqqqqq",
            "qqqqqq",
            json!({"include_offsets": "utf-8", "max_chars": 4})
        )
        .await,
        json!([{"text": "<em>qqqq</em>", "fragment_range": [2, 6], "match_ranges": [[2, 6]]}])
    );
}

#[tokio::test]
async fn structured_highlights_read_string_arrays() {
    // Element 1 is a number, skipped. Element 2 scores 2 and ranks first; element 3 scores 1;
    // element 0 scores nothing. Offsets restart in each element.
    let docs = json!([{"id": "r", "vector": [1.0, 0.5], "text": "z",
                       "attributes": {"tags": ["a b", 7, "q q", "q x"]}}]);
    let h = json!({"attr": "tags", "fragment_limit": 16, "include_offsets": "codepoints"});
    assert_eq!(
        structured_of(docs, "q", h).await,
        json!([
            {"text": "<em>q</em> <em>q</em>", "array_index": 2,
             "fragment_range": [0, 3], "match_ranges": [[0, 1], [2, 3]]},
            {"text": "<em>q</em> x", "array_index": 3,
             "fragment_range": [0, 3], "match_ranges": [[0, 1]]}
        ])
    );
}

#[tokio::test]
async fn structured_highlights_replace_the_string() {
    let a = api();
    let (s, b) = send(
        &a,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": [
            {"id": "r", "vector": [1.0, 0.5], "text": "run far", "attributes": {"label": 7}}]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // Each of the three keys alone makes it structured.
    for h in [
        json!({"fragment_by": "word"}),
        json!({"fragment_limit": 1}),
        json!({"include_offsets": "utf-8"}),
    ] {
        let b = query(&a, &json!({"text": "run", "top_k": 5, "highlight": h})).await;
        let r = &b["results"][0];
        assert!(r.get("$highlight").is_none(), "{h}: {b}");
        assert_eq!(r["$highlights"][0]["text"], "<em>run</em> far", "{h}: {b}");
    }
    // Unstructured: M63's string, and no list.
    let b = query(&a, &json!({"text": "run", "top_k": 5, "highlight": {}})).await;
    assert_eq!(b["results"][0]["$highlight"], "<em>run</em> far", "{b}");
    assert!(b["results"][0].get("$highlights").is_none(), "{b}");
    // A missing or numeric attribute gives neither.
    for attr in ["label", "nothing"] {
        let h = json!({"attr": attr, "fragment_limit": 2});
        let b = query(&a, &json!({"text": "run", "top_k": 5, "highlight": h})).await;
        let r = &b["results"][0];
        assert!(
            r.get("$highlight").is_none() && r.get("$highlights").is_none(),
            "{b}"
        );
    }
    // Per sub-query in a multi-query.
    let multi = json!({"queries": [
        {"text": "run", "top_k": 5, "highlight": {"fragment_limit": 1}},
        {"text": "run", "top_k": 5, "highlight": {}}]});
    let b = query(&a, &multi).await;
    let subs = b["results"].as_array().unwrap();
    assert_eq!(
        subs[0][0]["$highlights"],
        texts(&["<em>run</em> far"]),
        "{b}"
    );
    assert!(subs[0][0].get("$highlight").is_none(), "{b}");
    assert_eq!(subs[1][0]["$highlight"], "<em>run</em> far", "{b}");
    assert!(subs[1][0].get("$highlights").is_none(), "{b}");
}

#[tokio::test]
async fn structured_highlights_are_refused_at_the_door() {
    let a = api();
    put(&a, &[("r", "run")], None).await;
    let q = |h: Value| json!({"text": "run", "top_k": 5, "highlight": h});
    for (what, h) in [
        ("an unknown fragment_by", json!({"fragment_by": "line"})),
        ("a null fragment_by", json!({"fragment_by": null})),
        (
            "an unknown include_offsets",
            json!({"include_offsets": "utf-32"}),
        ),
        ("a null include_offsets", json!({"include_offsets": null})),
        ("fragment_limit 17", json!({"fragment_limit": 17})),
        ("fragment_limit -1", json!({"fragment_limit": -1})),
        (
            "a fractional fragment_limit",
            json!({"fragment_limit": 1.0}),
        ),
        ("a null fragment_limit", json!({"fragment_limit": null})),
        ("rank_fragments_by", json!({"rank_fragments_by": "score"})),
    ] {
        let (s, b) = send(&a, "POST", "/v1/indexes/docs/query", Some(&q(h))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{what}: {b}");
        assert!(
            b["error"]["message"]
                .as_str()
                .unwrap()
                .contains("highlight"),
            "{what}: the message names highlight: {b}"
        );
    }
    for h in [
        json!({"fragment_limit": 0}),
        json!({"fragment_limit": 16}),
        json!({"fragment_by": "none"}),
        json!({"fragment_by": "sentence"}),
        json!({"fragment_by": "paragraph"}),
        json!({"fragment_by": "word"}),
        json!({"include_offsets": "utf-8"}),
        json!({"include_offsets": "utf-16"}),
        json!({"include_offsets": "codepoints"}),
    ] {
        let r = query(&a, &q(h.clone())).await;
        assert!(r["results"][0]["$highlights"].is_array(), "{h}: {r}");
    }
}

#[tokio::test]
async fn a_structured_highlight_costs_nothing() {
    use pstore_testkit::depth::DepthCounting;
    let store = DepthCounting::new(MemoryStore::new());
    let cold = || Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    let writer = cold();
    for k in 0..4 {
        let rows: Vec<(String, String)> = (0..10)
            .map(|i| (format!("d{k}{i}"), format!("common word{i}. run tail{k}")))
            .collect();
        let refs: Vec<(&str, &str)> = rows.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        put(&writer, &refs, None).await;
        fold(&writer).await;
    }
    let mut seen = Vec::new();
    for body in [
        json!({"text": "common run", "top_k": 20}),
        json!({"text": "common run", "top_k": 20, "highlight": {
            "fragment_by": "sentence", "fragment_limit": 16, "include_offsets": "utf-16"}}),
    ] {
        let api = cold();
        store.reset();
        let b = query(&api, &body).await;
        assert_eq!(b["results"].as_array().unwrap().len(), 20);
        let cost = &b["meta"]["cost"];
        seen.push((
            store.depth(),
            cost["blob_reads"].clone(),
            cost["bytes_read"].clone(),
        ));
    }
    assert!(seen[0].0 > 0, "{seen:?}");
    assert_eq!(seen[0], seen[1], "plain, then structured: {seen:?}");
}
