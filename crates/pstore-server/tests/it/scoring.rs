//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M182: scoring expressions in `rank_by`, over HTTP.

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

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "182")
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

async fn query(api: &A, body: &Value) -> (StatusCode, Value) {
    send(api, "POST", "/v1/indexes/s/query", body).await
}

async fn ok(api: &A, body: &Value) -> Value {
    let (s, b) = query(api, body).await;
    assert_eq!(s, StatusCode::OK, "{body}: {b}");
    b
}

/// The answer's rows, in order, with their scores.
fn rows(b: &Value) -> Vec<(String, f64)> {
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

const DOCS: [(&str, &str, &str, &str); 8] = [
    (
        "whale facts and more facts",
        "about whales",
        "a huge whale swims",
        "whale",
    ),
    ("the quick fox", "a fox", "quick quick fox", "fox"),
    ("quick brown dogs", "", "the fox is quick", "dog"),
    ("whale", "whale facts", "huge", "whale"),
    ("nothing here", "nor here", "none at all", "cat"),
    (
        "foxes run quickly",
        "fast fox facts",
        "a whale of a time",
        "fox",
    ),
    ("", "", "", "whale"),
    ("quick", "huge whale facts", "fox fox fox", "dog"),
];

async fn index() -> A {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    let docs: Vec<Value> = DOCS
        .iter()
        .enumerate()
        .map(|(i, (title, description, content, species))| {
            let mut attrs = json!({"species": species, "n": i});
            for (k, v) in [
                ("title", title),
                ("description", description),
                ("content", content),
            ] {
                if !v.is_empty() {
                    attrs[k] = json!(v);
                }
            }
            json!({"id": format!("d{i}"), "vector": [1.0, i as f32], "attributes": attrs})
        })
        .collect();
    let schema = json!({
        "title": {"type": "string",
                  "full_text_search": {"language": "english", "stemming": true}},
        "description": {"type": "string", "full_text_search": true},
        "content": {"type": "string", "full_text_search": {"k1": 1.5, "b": 0.6}},
    });
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/s/documents",
        &json!({"durability": "durable", "schema": schema, "documents": docs}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    api
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// A text leg's BM25 scores by id: its definition's BM25 term.
async fn leg(api: &A, field: &str, q: &str) -> BTreeMap<String, f64> {
    let b = ok(
        api,
        &json!({"text": [{"field": field, "query": q}],
                "fusion": {"sum": {"weights": [1.0]}}, "top_k": 100}),
    )
    .await;
    rows(&b).into_iter().collect()
}

/// `got` is `want`'s positive rows, each within 1e-5 relative, by score then id.
fn assert_defined(got: &[(String, f64)], want: &BTreeMap<String, f64>, what: &str) {
    let mut want: Vec<(&String, f64)> = want
        .iter()
        .filter(|(_, s)| **s > 0.0)
        .map(|(id, s)| (id, *s))
        .collect();
    let mut ids: Vec<&String> = got.iter().map(|(id, _)| id).collect();
    ids.sort();
    let mut want_ids: Vec<&String> = want.iter().map(|(id, _)| *id).collect();
    want_ids.sort();
    assert_eq!(ids, want_ids, "{what}");
    want.sort_by(|a, b| b.1.total_cmp(&a.1));
    let by: BTreeMap<&String, f64> = want.iter().copied().collect();
    for (id, s) in got {
        assert!(
            (s - by[id]).abs() <= 1e-5 * by[id],
            "{id}: {s} against {}: {what}",
            by[id]
        );
    }
    for w in got.windows(2) {
        assert!(
            w[0].1 > w[1].1 || (w[0].1 == w[1].1 && w[0].0 < w[1].0),
            "{w:?}: {what}"
        );
    }
}

fn rank(e: &Value) -> Value {
    json!({"rank_by": e, "top_k": 100})
}

/// The vendor's examples, each against its definition from the text legs, before and after a
/// fold; the two answers equal.
async fn examples(api: &A) -> Vec<Value> {
    let mut answers = Vec::new();
    // "Operators can be nested."
    let e = json!([
        "Sum",
        [
            [
                "Max",
                [
                    ["title", "BM25", "whale facts"],
                    ["description", "BM25", "whale facts"]
                ]
            ],
            ["content", "BM25", "huge whale"]
        ]
    ]);
    let (t, d, c) = (
        leg(api, "title", "whale facts").await,
        leg(api, "description", "whale facts").await,
        leg(api, "content", "huge whale").await,
    );
    let want: BTreeMap<String, f64> = (0..DOCS.len())
        .map(|i| {
            let id = format!("d{i}");
            let at = |m: &BTreeMap<String, f64>| m.get(&id).copied().unwrap_or(0.0);
            let s = at(&t).max(at(&d)) + at(&c);
            (id, s)
        })
        .collect();
    let b = ok(api, &rank(&e)).await;
    assert_defined(&rows(&b), &want, "nested");
    assert!(rows(&b).len() >= 4, "{b}");
    answers.push(b["results"].clone());

    // "A weight / boost per-field by using the Product operator", both forms.
    let t = leg(api, "title", "quick fox").await;
    let c = leg(api, "content", "quick fox").await;
    let want: BTreeMap<String, f64> = (0..DOCS.len())
        .map(|i| {
            let id = format!("d{i}");
            let at = |m: &BTreeMap<String, f64>| m.get(&id).copied().unwrap_or(0.0);
            let s = 2.0 * at(&t) + at(&c);
            (id, s)
        })
        .collect();
    let forward = ok(
        api,
        &rank(&json!([
            "Sum",
            [
                ["Product", 2, ["title", "BM25", "quick fox"]],
                ["content", "BM25", "quick fox"]
            ]
        ])),
    )
    .await;
    let reversed = ok(
        api,
        &rank(&json!([
            "Sum",
            [
                ["Product", ["title", "BM25", "quick fox"], 2],
                ["content", "BM25", "quick fox"]
            ]
        ])),
    )
    .await;
    assert_defined(&rows(&forward), &want, "Product");
    assert_eq!(
        forward["results"], reversed["results"],
        "both Product forms"
    );
    answers.push(forward["results"].clone());

    // "Filters can be used inside rank_by expressions."
    let want: BTreeMap<String, f64> = (0..DOCS.len())
        .map(|i| {
            let id = format!("d{i}");
            let whale = f64::from(u8::from(DOCS[i].3 == "whale"));
            (id.clone(), t.get(&id).copied().unwrap_or(0.0) + whale)
        })
        .collect();
    let b = ok(
        api,
        &rank(&json!([
            "Sum",
            [["title", "BM25", "quick fox"], ["species", "Eq", "whale"]]
        ])),
    )
    .await;
    assert_defined(&rows(&b), &want, "filter in Sum");
    answers.push(b["results"].clone());
    let b = ok(
        api,
        &rank(&json!(["Product", 2.0, ["species", "Eq", "whale"]])),
    )
    .await;
    assert_eq!(
        rows(&b),
        [("d0", 2.0), ("d3", 2.0), ("d6", 2.0)].map(|(i, s)| (i.to_owned(), s)),
        "{b}"
    );
    answers.push(b["results"].clone());

    // `filters` restrict the scored rows; `offset` pages.
    let e = json!([
        "Sum",
        [["content", "BM25", "fox whale"], ["title", "BM25", "quick"]]
    ]);
    let all = rows(&ok(api, &rank(&e)).await);
    assert!(all.len() >= 4, "{all:?}");
    let some = rows(
        &ok(
            api,
            &json!({"rank_by": e, "top_k": 100, "filters": ["species", "In", ["fox", "dog"]]}),
        )
        .await,
    );
    let kept: Vec<(String, f64)> = all
        .iter()
        .filter(|(id, _)| {
            let i: usize = id[1..].parse().unwrap();
            ["fox", "dog"].contains(&DOCS[i].3)
        })
        .cloned()
        .collect();
    assert_eq!(some, kept, "filters");
    let page = rows(&ok(api, &json!({"rank_by": e, "top_k": 2, "offset": 1})).await);
    assert_eq!(page, all[1..3], "offset");
    answers.push(json!(
        all.iter().map(|(id, s)| json!([id, s])).collect::<Vec<_>>()
    ));
    answers
}

/// Criterion 4.
#[tokio::test]
async fn scoring_expressions_over_http() {
    let api = index().await;
    let before = examples(&api).await;
    fold(&api).await;
    let after = examples(&api).await;
    assert_eq!(before, after, "unfolded rows score as they do folded");

    // An M140 order pair, and a list of them, are still orders: no score.
    let b = ok(&api, &json!({"rank_by": ["n", "desc"], "top_k": 2})).await;
    assert_eq!(b["results"][0]["id"], "d7", "{b}");
    assert!(b["results"][0]["score"].is_null(), "{b}");
    let b = ok(
        &api,
        &json!({"rank_by": [["species", "asc"], ["n", "desc"]], "top_k": 1}),
    )
    .await;
    assert_eq!(b["results"][0]["id"], "d4", "{b}");

    // Delta 6's refusals.
    let bm = json!(["title", "BM25", "fox"]);
    let deep = (0..8).fold(bm.clone(), |e, _| json!(["Product", 1, e]));
    let wide = json!([
        "Sum",
        vec![json!(["Sum", [bm.clone(), bm.clone(), bm.clone()]]); 16]
    ]);
    let refused = [
        json!(["Product", -1, bm]),
        json!(["Product", "2", bm]),
        json!(["Product", 1e4, bm]),
        json!(["Product", 1e-4, bm]),
        json!(["Product", 1e-50, bm]),
        json!(["Product", 2, bm, 3]),
        json!(["Product", bm, bm]),
        json!(["Sum", []]),
        json!(["Max", vec![bm.clone(); 17]]),
        json!(["Sum", bm]),
        wide,
        deep,
        json!(["species", "BM25", "fox"]),
        json!(["nope", "BM25", "fox"]),
        json!(["title", "BM25", "fox", {"last_as_prefix": "yes"}]),
        json!(["title", "BM25", "fox", {"prefix": true}]),
        json!(["title", "BM25", 7]),
        json!(["title", "Nope", "fox"]),
    ];
    for e in refused {
        let (s, b) = query(&api, &rank(&e)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{e}: {b}");
    }
    let beside = [
        ("vector", json!([1.0, 0.0])),
        ("multi", json!([[1.0, 0.0]])),
        ("text", json!("fox")),
        ("field", json!("vector")),
        ("exact", json!(true)),
        ("fusion", json!({"sum": {"weights": [1.0]}})),
        ("boost", json!({"attribute": "n"})),
        ("aggregate_by", json!({"c": ["Count", "id"]})),
        ("highlight", json!(true)),
    ];
    for (k, v) in beside {
        let mut body = rank(&bm);
        body[k] = v;
        let (s, b) = query(&api, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{k}: {b}");
    }
    let (s, b) = query(&api, &json!({"rank_by": bm, "top_k": 9_999, "offset": 2})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    // A weight at either bound, and 0, are accepted.
    for w in [json!(0), json!(1e-3), json!(1000)] {
        ok(&api, &rank(&json!(["Product", w, bm]))).await;
    }
}
