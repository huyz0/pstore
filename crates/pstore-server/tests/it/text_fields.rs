//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M155: several BM25 text fields over HTTP. A further field is declared with
//! `full_text_search` on any attribute, queried by `{"field", "query"}` legs, and ranks as an
//! index whose primary holds it would.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
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
        .header("x-pstore-tenant", "155")
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

fn title_fts() -> Value {
    json!({"language": "english", "stemming": true, "k1": 1.5, "b": 0.6})
}

/// Criterion 1's declarations on `multi`.
fn declared() -> Value {
    json!({
        "title": {"type": "string", "full_text_search": title_fts()},
        "tags": {"type": "[]string", "full_text_search": true},
    })
}

const TITLES: [&str; 7] = [
    "quarterly report",
    "run the numbers",
    "running late again",
    "a quarterly run of reports",
    "runs and runners",
    "late report",
    "growth",
];
const TAGS: [&str; 6] = ["rust", "db", "index", "search", "rust db", "storage"];
const TEXTS: [&str; 5] = [
    "alpha beta",
    "beta gamma delta",
    "gamma",
    "alpha alpha gamma",
    "delta epsilon beta",
];

/// `n` words no query names and no other row holds, so rows differ in length, and so in score.
fn filler(i: usize, field: &str, n: usize) -> String {
    (0..n).map(|k| format!(" {field}{i}w{k}")).collect()
}

/// Row `i`'s attributes on `multi`: `text` but every fifth, `title` but every third, and
/// `tags` an array of 0 to 3 elements, `[]` every fourth. Each value is padded by `filler`
/// by a different modulus, so no two rows tie on the queries below.
fn attrs(i: usize) -> Map<String, Value> {
    let mut m = Map::new();
    if !i.is_multiple_of(5) {
        let t = TEXTS[(i * 3) % TEXTS.len()];
        m.insert(
            "text".into(),
            json!(format!("{t}{}", filler(i, "x", i % 13))),
        );
    }
    if !i.is_multiple_of(3) {
        let t = TITLES[(i * 5 + i / 7) % TITLES.len()];
        m.insert(
            "title".into(),
            json!(format!("{t}{}", filler(i, "t", i % 9))),
        );
    }
    let tags: Vec<String> = if i.is_multiple_of(4) {
        Vec::new()
    } else {
        (0..1 + i % 3)
            .map(|k| TAGS[(i * 7 + k * 5 + i / 4) % TAGS.len()].to_owned())
            .chain(std::iter::once(filler(i, "g", i % 11).trim().to_owned()))
            .filter(|t| !t.is_empty())
            .collect()
    };
    m.insert("tags".into(), json!(tags));
    m
}

const ROWS: usize = 40;

fn doc(i: usize, attributes: Map<String, Value>) -> Value {
    json!({"id": format!("d{i:02}"), "vector": [1.0, (i as f32) / 40.0], "attributes": attributes})
}

/// `multi`, and each further field's twin: the same ids with `text` set to the field.
async fn world(api: &A) {
    let docs: Vec<Value> = (0..ROWS).map(|i| doc(i, attrs(i))).collect();
    let (s, b) = write(
        api,
        "multi",
        &json!({"durability": "durable", "schema": declared(), "documents": docs}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for (field, twin, fts) in [
        ("title", "twin_title", title_fts()),
        ("tags", "twin_tags", json!(true)),
    ] {
        let docs: Vec<Value> = (0..ROWS)
            .map(|i| {
                let mut m = Map::new();
                if let Some(v) = attrs(i).remove(field) {
                    m.insert("text".into(), v);
                }
                doc(i, m)
            })
            .collect();
        let schema = json!({"text": {"type": "[]string", "full_text_search": fts}});
        let (s, b) = write(
            api,
            twin,
            &json!({"durability": "durable", "schema": schema, "documents": docs}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
}

async fn query(api: &A, index: &str, body: &Value) -> (StatusCode, Value) {
    send(api, "POST", &format!("/v1/indexes/{index}/query"), body).await
}

/// A query's results: each id and score, in order. A query naming no fusion is given `Sum`
/// with weight 1 over its one leg, so its scores are BM25's rather than RRF's ranks.
async fn ranked(api: &A, index: &str, mut body: Value) -> Vec<(String, f64)> {
    if body.get("fusion").is_none() {
        body["fusion"] = json!({"sum": {"weights": [1.0]}});
    }
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

fn leg(field: &str, q: &str) -> Value {
    json!({"field": field, "query": q})
}

const ONE: [(&str, &str, &str); 6] = [
    ("title", "twin_title", "Running"),
    ("title", "twin_title", "quarterly report"),
    ("tags", "twin_tags", "rust"),
    ("tags", "twin_tags", "db index"),
    ("text", "multi", "alpha"),
    ("text", "multi", "beta gamma"),
];

#[tokio::test]
async fn one_further_field_ranks_as_its_own_index() {
    // Criterion 1.
    let a = api();
    world(&a).await;
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        for (field, twin, q) in ONE {
            let got = ranked(&a, "multi", json!({"text": [leg(field, q)], "top_k": 100})).await;
            let want = ranked(&a, twin, json!({"text": q, "top_k": 100})).await;
            assert!(
                !got.is_empty(),
                "{field} {q:?} found nothing, folded: {folded}"
            );
            assert_eq!(got, want, "{field} {q:?}, folded: {folded}");
        }
        // Stemming is `title`'s: "Running" finds a row holding "run" alone.
        let run = ranked(
            &a,
            "multi",
            json!({"text": [leg("title", "Running")], "top_k": 100}),
        )
        .await;
        let holds_run = (0..ROWS)
            .filter(|i| {
                attrs(*i)
                    .get("title")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.starts_with("run the numbers"))
            })
            .map(|i| format!("d{i:02}"))
            .next()
            .unwrap();
        assert!(run.iter().any(|(id, _)| *id == holds_run), "{run:?}");
    }
}

/// The 10 best of `legs`' weighted twin scores, combined by `combine`, a row absent from a
/// twin's answer adding nothing there; and the 11th best, to check for near ties.
fn expected(
    twins: &[Vec<(String, f64)>],
    weights: &[f64],
    combine: fn(f64, f64) -> f64,
) -> Vec<(String, f64)> {
    let mut all: BTreeMap<String, f64> = BTreeMap::new();
    for (answer, w) in twins.iter().zip(weights) {
        for (id, s) in answer {
            let e = all.entry(id.clone()).or_insert(0.0);
            *e = combine(*e, w * s);
        }
    }
    let mut v: Vec<(String, f64)> = all.into_iter().collect();
    v.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(&y.0)));
    v.truncate(11);
    v
}

#[tokio::test]
async fn several_fields_fuse_as_their_twins() {
    // Criterion 2.
    let a = api();
    world(&a).await;
    let (tq, gq, pq) = ("running report", "rust storage", "gamma delta");
    let weights = [1.0, 0.5, 2.0];
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        // `Sum`'s legs are whole (M9g.2), so its twins are unlimited; `Max`'s keep their
        // limit, `top_k`.
        for (kind, combine, cut) in [
            ("sum", (|x, y| x + y) as fn(f64, f64) -> f64, 100),
            ("max", f64::max, 10),
        ] {
            let twins = [
                ranked(&a, "twin_title", json!({"text": tq, "top_k": cut})).await,
                ranked(&a, "twin_tags", json!({"text": gq, "top_k": cut})).await,
                ranked(&a, "multi", json!({"text": pq, "top_k": cut})).await,
            ];
            let want = expected(&twins, &weights, combine);
            assert_eq!(want.len(), 11, "{kind}: too few candidates");
            for pair in want.windows(2) {
                assert!(
                    pair[0].1 - pair[1].1 > 1e-5,
                    "{kind}: a near tie among the top 11: {want:?}"
                );
            }
            let body = json!({
                "text": [leg("title", tq), leg("tags", gq), pq],
                "fusion": {kind: {"weights": weights}},
                "top_k": 10,
            });
            let got = ranked(&a, "multi", body).await;
            assert_eq!(got.len(), 10, "{kind}");
            for ((gid, gs), (wid, ws)) in got.iter().zip(&want) {
                assert_eq!(
                    gid, wid,
                    "{kind}, folded {folded}: {got:?} against {want:?}"
                );
                assert!((gs - ws).abs() <= 1e-5, "{kind} {gid}: {gs} against {ws}");
            }
        }
    }
}

#[tokio::test]
async fn what_a_further_field_cannot_be_is_refused_at_the_door() {
    // Criterion 3.
    let a = api();
    let d = |attrs: Value| json!([{"id": "x", "vector": [1.0, 0.5], "attributes": attrs}]);
    let nine: Map<String, Value> = (0..9)
        .map(|i| (format!("f{i}"), json!({"full_text_search": true})))
        .collect();
    let cases = [
        (json!(nine), d(json!({"f0": "a"})), "at most 8"),
        (
            json!({"$x": {"full_text_search": true}}),
            d(json!({})),
            "reserved",
        ),
        (
            json!({"id": {"full_text_search": true}}),
            d(json!({})),
            "reserved",
        ),
        (
            json!({"title": {"type": "int", "full_text_search": true}}),
            d(json!({"title": "a"})),
            "type is string or []string",
        ),
        (
            json!({"title": {"full_text_search": {"k1": 4}}}),
            d(json!({"title": "a"})),
            "k1 must be",
        ),
        (
            json!({"title": {"full_text_search": {"tokenizer": "x"}}}),
            d(json!({"title": "a"})),
            "tokenizer",
        ),
        (declared(), d(json!({"title": 5})), "`title`"),
        (declared(), d(json!({"tags": ["a", 5]})), "`tags`"),
    ];
    for (schema, docs, why) in cases {
        let body = json!({"durability": "durable", "schema": schema, "documents": docs});
        let (s, b) = write(&a, "multi2", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} was stored: {b}");
        let msg = b["error"]["message"].as_str().unwrap();
        assert!(msg.contains(why), "{body}: {msg:?} does not name {why:?}");
    }
    let (s, b) = send(&a, "GET", "/v1/indexes/multi2", &json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn further_fields_are_fixed_when_the_index_is_created() {
    // Criterion 4: against the pending rows, then against the HEAD.
    let a = api();
    world(&a).await;
    let one = |i: usize| json!([doc(i, attrs(i))]);
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        let other =
            json!({"title": {"full_text_search": true}, "tags": {"full_text_search": true}});
        let tags_alone = json!({"tags": {"type": "[]string", "full_text_search": true}});
        for schema in [other, tags_alone] {
            let body = json!({"durability": "durable", "schema": schema, "documents": one(1)});
            let (s, b) = write(&a, "multi", &body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{body}, folded {folded}: {b}");
            assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
            let msg = b["error"]["message"].as_str().unwrap();
            assert!(
                msg.contains("the further text fields") && msg.contains("reindex"),
                "{msg}"
            );
        }
        for body in [
            json!({"durability": "durable", "documents": one(2)}),
            json!({"durability": "durable", "schema": declared(), "documents": one(4)}),
        ] {
            let (s, b) = write(&a, "multi", &body).await;
            assert_eq!(s, StatusCode::OK, "{body}, folded {folded}: {b}");
        }
    }
}

async fn refused(api: &A, index: &str, body: Value, code: &str, why: &str) -> String {
    let (s, b) = query(api, index, &body).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body} answered: {b}");
    assert_eq!(b["error"]["code"], code, "{body}: {b}");
    let msg = b["error"]["message"].as_str().unwrap().to_owned();
    assert!(msg.contains(why), "{body}: {msg:?} does not name {why:?}");
    msg
}

#[tokio::test]
async fn the_query_syntax_and_its_refusals() {
    // Criterion 5.
    let a = api();
    world(&a).await;
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        for t in [
            json!([{"field": "title"}]),
            json!([{"field": "title", "query": 5}]),
            json!([{"field": "title", "query": "a", "limit": 3}]),
            json!([]),
        ] {
            refused(&a, "multi", json!({"text": t}), "bad_request", "text").await;
        }
        let msg = refused(
            &a,
            "multi",
            // `zzz` sorts after `title`, so naming the first queried field would name `title`.
            json!({"text": [leg("title", "run"), leg("zzz", "run")]}),
            "unknown_field",
            "text field `zzz`",
        )
        .await;
        assert!(!msg.contains("title"), "{msg}");
        refused(
            &a,
            "multi",
            json!({"vector": [1.0, 0.5], "text": [leg("nosuch", "run")]}),
            "unknown_field",
            "text field `nosuch`",
        )
        .await;
        refused(
            &a,
            "multi",
            json!({"text": [leg("$x", "run")]}),
            "unknown_field",
            "text field `$x`",
        )
        .await;
        refused(
            &a,
            "multi",
            json!({"text": [leg("title", "run")], "highlight": {}}),
            "bad_request",
            "title",
        )
        .await;
        let (s, b) = query(
            &a,
            "multi",
            &json!({"text": [leg("text", "alpha")], "highlight": {}, "top_k": 3}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(
            b["results"][0]["$highlight"]
                .as_str()
                .is_some_and(|h| h.contains("<em>alpha</em>")),
            "the primary's leg was not marked: {b}"
        );
    }
    // An index whose rows hold `title` and never `text`.
    let docs: Vec<Value> = (1..6)
        .map(|i| doc(i, json!({"title": TITLES[i]}).as_object().unwrap().clone()))
        .collect();
    let schema = json!({"title": {"full_text_search": true}});
    let (s, b) = write(
        &a,
        "titles",
        &json!({"durability": "durable", "schema": schema, "documents": docs}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        refused(
            &a,
            "titles",
            json!({"text": "report"}),
            "unknown_field",
            "text field `text`",
        )
        .await;
        let hits = ranked(&a, "titles", json!({"text": [leg("title", "report")]})).await;
        assert!(!hits.is_empty(), "folded {folded}");
    }
}

#[tokio::test]
async fn the_index_reports_its_further_fields() {
    // Criterion 6.
    let a = api();
    world(&a).await;
    fold(&a).await;
    let (s, b) = send(&a, "GET", "/v1/indexes/multi", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let fields = b["schema"]["text_fields"].clone();
    // Each as the primary's `full_text_search` reports the same declaration on its twin.
    let mut want = Map::new();
    for (field, twin) in [("title", "twin_title"), ("tags", "twin_tags")] {
        let (s, t) = send(&a, "GET", &format!("/v1/indexes/{twin}"), &json!({})).await;
        assert_eq!(s, StatusCode::OK, "{t}");
        want.insert(field.into(), t["schema"]["full_text_search"].clone());
    }
    assert_eq!(fields, Value::Object(want), "{b}");
    assert_eq!(fields["title"]["stemming"], true, "{b}");
    let (s, b) = send(&a, "GET", "/v1/indexes/twin_tags", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["schema"].get("text_fields").is_none(), "{b}");
}
