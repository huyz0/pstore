//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M158: the `pre_tokenized_array` tokenizer over HTTP. Each string of a `[]string` value is
//! one token, kept byte for byte, so a pre-tokenized field ranks and filters as a twin whose
//! every distinct element is one plain word.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

fn api() -> A {
    Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap()
}

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "158")
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

async fn write(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    send(api, "PUT", &format!("/v1/indexes/{index}/documents"), body).await
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn pre() -> Value {
    json!({"tokenizer": "pre_tokenized_array"})
}

/// Multi-word, mixed-case elements. None is another's lowercase, and none is "New" alone.
const POOL: [&str; 8] = [
    "New York",
    "nyc",
    "New Yorker",
    "Big Apple",
    "Los Angeles",
    "la",
    "New Jersey",
    "NYC",
];

const ROWS: usize = 36;

/// Row `i`'s tags: one to four elements of `POOL`, and a filler element no other row holds,
/// every third row, so rows differ in length. Rows 0 to 2 are placed by hand: "New York" then
/// "nyc" consecutive, in that order, both but apart, and both in the other order.
fn tags(i: usize) -> Vec<String> {
    let fixed: &[&str] = match i {
        0 => &["Big Apple", "New York", "nyc"],
        1 => &["New York", "la", "nyc"],
        2 => &["nyc", "New York"],
        _ => &[],
    };
    let mut t: Vec<String> = if fixed.is_empty() {
        (0..1 + i % 4)
            .map(|k| POOL[(i * 5 + k * 3 + i / 5) % POOL.len()].to_owned())
            .collect()
    } else {
        fixed.iter().map(|s| (*s).to_owned()).collect()
    };
    if i.is_multiple_of(3) {
        t.push(format!("Filler {i}"));
    }
    t
}

/// Each distinct element as one plain word, `tok0` and up, in sorted order.
fn words() -> BTreeMap<String, String> {
    let all: BTreeSet<String> = (0..ROWS)
        .flat_map(tags)
        .chain(POOL.map(String::from))
        .collect();
    all.into_iter()
        .enumerate()
        .map(|(k, s)| (s, format!("tok{k}")))
        .collect()
}

fn mapped(ts: &[&str]) -> String {
    let w = words();
    ts.iter()
        .map(|t| w[*t].clone())
        .collect::<Vec<_>>()
        .join(" ")
}

fn doc(i: usize, field: &str, value: Value) -> Value {
    json!({
        "id": format!("d{i:02}"),
        "vector": [1.0, (i as f32) / 36.0],
        "attributes": {field: value},
    })
}

/// `pre`, its `twin`, and `prim`, whose primary is `pre`'s `tags`.
async fn world(api: &A) {
    let w = words();
    let cases = [
        (
            "pre",
            json!({"tags": {"type": "[]string", "full_text_search": pre()}}),
            "tags",
            false,
        ),
        (
            "twin",
            json!({"tags": {"type": "[]string", "full_text_search": true}}),
            "tags",
            true,
        ),
        (
            "prim",
            json!({"text": {"type": "[]string", "full_text_search": pre()}}),
            "text",
            false,
        ),
    ];
    for (index, schema, field, map) in cases {
        let docs: Vec<Value> = (0..ROWS)
            .map(|i| {
                let t: Vec<String> = tags(i)
                    .into_iter()
                    .map(|s| if map { w[&s].clone() } else { s })
                    .collect();
                doc(i, field, json!(t))
            })
            .collect();
        let body = json!({"durability": "durable", "schema": schema, "documents": docs});
        let (s, b) = write(api, index, &body).await;
        assert_eq!(s, StatusCode::OK, "{index}: {b}");
    }
}

async fn query(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    send(api, "POST", &format!("/v1/indexes/{index}/query"), body).await
}

/// A text query's results, each id and score in order, under `Sum` with weight 1.
async fn ranked(api: &A, index: &str, field: &str, q: Value) -> Vec<(String, f64)> {
    let body = json!({
        "text": [{"field": field, "query": q}],
        "fusion": {"sum": {"weights": [1.0]}},
        "top_k": 100,
    });
    let (s, b) = query(api, index, &body).await;
    assert_eq!(s, StatusCode::OK, "{index} {body} -> {b}");
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

/// The ids a vector query admits under `filter`.
async fn admitted(api: &A, index: &str, filter: Value) -> BTreeSet<String> {
    let body = json!({"vector": [1.0, 0.5], "filters": filter, "top_k": 100});
    let (s, b) = query(api, index, &body).await;
    assert_eq!(s, StatusCode::OK, "{index} {body} -> {b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn ids(pred: impl Fn(&[String]) -> bool) -> BTreeSet<String> {
    (0..ROWS)
        .filter(|i| pred(&tags(*i)))
        .map(|i| format!("d{i:02}"))
        .collect()
}

#[tokio::test]
async fn a_pre_tokenized_field_ranks_as_its_tokens() {
    // Criterion 2.
    let a = api();
    world(&a).await;
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        for q in [
            &["New York", "nyc"][..],
            &["Big Apple"],
            &["la", "New Jersey", "NYC"],
        ] {
            let got = ranked(&a, "pre", "tags", json!(q)).await;
            let want = ranked(&a, "twin", "tags", json!(mapped(q))).await;
            assert!(!got.is_empty(), "{q:?} found nothing, folded {folded}");
            assert_eq!(got, want, "{q:?}, folded {folded}");
            let prim = ranked(&a, "prim", "text", json!(q)).await;
            assert_eq!(prim, got, "prim {q:?}, folded {folded}");
        }
        for q in [json!(["new york"]), json!(["New"]), json!("New")] {
            let got = ranked(&a, "pre", "tags", q.clone()).await;
            assert!(got.is_empty(), "{q} matched {got:?}, folded {folded}");
        }
        // A string operand is one token.
        let one = ranked(&a, "pre", "tags", json!("New York")).await;
        let arr = ranked(&a, "pre", "tags", json!(["New York"])).await;
        assert!(!one.is_empty());
        assert_eq!(one, arr, "folded {folded}");
    }
}

#[tokio::test]
async fn a_pre_tokenized_field_filters_as_its_tokens() {
    // Criterion 3.
    let a = api();
    world(&a).await;
    let has = |t: &[String], s: &str| t.iter().any(|e| e == s);
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        for (op, q) in [
            ("ContainsAllTokens", &["New York", "nyc"][..]),
            ("ContainsAnyToken", &["New York", "nyc"]),
            ("ContainsAnyToken", &["la", "Big Apple"]),
        ] {
            let got = admitted(&a, "pre", json!(["tags", op, q])).await;
            let want = admitted(&a, "twin", json!(["tags", op, mapped(q)])).await;
            assert!(!got.is_empty() && got.len() < ROWS, "{op} {q:?}: {got:?}");
            assert_eq!(got, want, "{op} {q:?}, folded {folded}");
        }
        let seq = admitted(
            &a,
            "pre",
            json!(["tags", "ContainsTokenSequence", ["New York", "nyc"]]),
        )
        .await;
        let want = ids(|t| t.windows(2).any(|w| w[0] == "New York" && w[1] == "nyc"));
        assert!(want.contains("d00"), "{want:?}");
        assert!(!want.contains("d01") && !want.contains("d02"), "{want:?}");
        assert_eq!(seq, want, "folded {folded}");
        let prefix = admitted(
            &a,
            "pre",
            json!(["tags", "ContainsAllTokens", ["New Y"], {"last_as_prefix": true}]),
        )
        .await;
        let want = ids(|t| t.iter().any(|e| e.starts_with("New Y")));
        assert!(!want.is_empty() && want.len() < ROWS, "{want:?}");
        assert!(
            want != ids(|t| has(t, "New York")),
            "no row holds New Yorker"
        );
        assert_eq!(prefix, want, "folded {folded}");
    }
}

#[tokio::test]
async fn what_a_pre_tokenized_field_cannot_be_is_refused() {
    // Criterion 4.
    let a = api();
    let d = |v: Value| json!([{"id": "x", "vector": [1.0, 0.5], "attributes": {"tags": v}}]);
    let with = |extra: Value| {
        let mut f = pre();
        f.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"tags": {"type": "[]string", "full_text_search": f}})
    };
    let ok = json!({"tags": {"type": "[]string", "full_text_search": pre()}});
    let untyped = json!({"tags": {"full_text_search": pre()}});
    let cases = [
        (with(json!({"language": "english"})), d(json!(["a"]))),
        (with(json!({"stemming": true})), d(json!(["a"]))),
        (with(json!({"remove_stopwords": true})), d(json!(["a"]))),
        (with(json!({"case_sensitive": false})), d(json!(["a"]))),
        (with(json!({"ascii_folding": true})), d(json!(["a"]))),
        // No document holds `tags`, so only the declaration is judged.
        (
            json!({"tags": {"type": "string", "full_text_search": pre()}}),
            json!([{"id": "x", "vector": [1.0, 0.5]}]),
        ),
        (ok.clone(), d(json!("a"))),
        (ok.clone(), d(json!(5))),
        (ok.clone(), d(json!(["a", 5]))),
        (ok.clone(), d(json!(["a\u{1F}b"]))),
        // Undeclared types, so only the tokenizer's own check refuses these.
        (untyped.clone(), d(json!("a"))),
        (untyped.clone(), d(json!(["a", 5]))),
        (untyped, d(json!(["a\u{1F}b"]))),
        (
            json!({"text": {"full_text_search": pre()}}),
            json!([{"id": "x", "vector": [1.0, 0.5], "attributes": {"text": ["a", 5]}}]),
        ),
        (
            json!({"text": {"full_text_search": pre()}}),
            json!([{"id": "x", "vector": [1.0, 0.5], "attributes": {"text": "a"}}]),
        ),
    ];
    for (schema, docs) in cases {
        let body = json!({"durability": "durable", "schema": schema, "documents": docs});
        let (s, b) = write(&a, "door", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} was stored: {b}");
    }
    let (s, b) = send(&a, "GET", "/v1/indexes/door", &json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");

    for (index, schema) in [
        (
            "ok1",
            json!({"tags": {"full_text_search":
                {"tokenizer": "pre_tokenized_array", "case_sensitive": true, "k1": 1.5}}}),
        ),
        (
            "ok2",
            json!({"tags": {"type": "[]string", "full_text_search":
                {"tokenizer": "pre_tokenized_array", "case_sensitive": true, "k1": 1.5}}}),
        ),
    ] {
        let body =
            json!({"durability": "durable", "schema": schema, "documents": d(json!(["a b"]))});
        let (s, b) = write(&a, index, &body).await;
        assert_eq!(s, StatusCode::OK, "{body}: {b}");
    }

    world(&a).await;
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        for body in [
            json!({"text": [{"field": "tags", "query": "a\u{1F}b"}]}),
            json!({"text": [{"field": "tags", "query": ["a", "b\u{1F}"]}]}),
            json!({"vector": [1.0, 0.5], "filters": ["tags", "ContainsAnyToken", "a\u{1F}b"]}),
            json!({"vector": [1.0, 0.5], "filters": ["tags", "ContainsAnyToken", ["a\u{1F}"]]}),
            json!({"text": [{"field": "text", "query": ["New York"]}], "highlight": {}}),
        ] {
            let index = if body.get("highlight").is_some() {
                "prim"
            } else {
                "pre"
            };
            let (s, b) = query(&a, index, &body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{index} {body} answered: {b}");
        }
    }
}

#[tokio::test]
async fn the_index_reports_its_tokenizer() {
    // Criterion 5.
    let a = api();
    world(&a).await;
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        let body = json!({
            "durability": "durable",
            "schema": {"tags": {"type": "[]string", "full_text_search": {"tokenizer": "word_v1"}}},
            "documents": [doc(1, "tags", json!(tags(1)))],
        });
        let (s, b) = write(&a, "pre", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}, folded {folded}: {b}");
        assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    }
    // The schema is reported once the index has a HEAD.
    {
        let (s, b) = send(&a, "GET", "/v1/indexes/pre", &json!({})).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let declared = &b["schema"]["text_fields"]["tags"];
        assert_eq!(declared["tokenizer"], "pre_tokenized_array", "{b}");
        assert_eq!(declared["case_sensitive"], true, "{b}");
        let (s, b) = send(&a, "GET", "/v1/indexes/twin", &json!({})).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            b["schema"]["text_fields"]["tags"]["tokenizer"], "word_v1",
            "{b}"
        );
    }
}
