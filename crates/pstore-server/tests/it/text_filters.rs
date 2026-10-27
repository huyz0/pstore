//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M156: a token filter over a further text field is analyzed by that field's analyzer, on
//! every path a filter is evaluated on, as it would be over an index whose primary held it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::BTreeSet;
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
        .header("x-pstore-tenant", "156")
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

async fn write(api: &A, index: &str, mut body: Value) {
    body["durability"] = json!("durable");
    let (s, b) = send(api, "PUT", &format!("/v1/indexes/{index}/documents"), &body).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn query(api: &A, index: &str, q: &Value) -> Value {
    let (s, b) = send(api, "POST", &format!("/v1/indexes/{index}/query"), q).await;
    assert_eq!(s, StatusCode::OK, "{index} {q} -> {b}");
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

fn stemming() -> Value {
    json!({"language": "english", "stemming": true})
}

/// `multi`'s declarations: the primary under the default analyzer, `title` stemmed.
fn declared() -> Value {
    json!({
        "text": {"type": "string", "full_text_search": true},
        "title": {"type": "string", "full_text_search": stemming()},
    })
}

/// Rows `(id, title, text)`. Each `text` is a word every analyzer here leaves alone.
const ROWS: [(&str, Option<&str>, &str); 12] = [
    ("r00", Some("run report"), "zebra"),
    ("r01", Some("run"), "quartz"),
    ("r02", Some("run late again"), "zebra"),
    ("r03", Some("run reporting"), "quartz"),
    ("r04", Some("running reports daily"), "zebra"),
    ("r05", Some("walk"), "quartz"),
    ("r06", Some("quarterly report"), "zebra"),
    ("r07", None, "quartz"),
    ("r08", Some("late run"), "zebra"),
    ("r09", Some("reports"), "quartz"),
    ("r10", Some("Running"), "zebra"),
    ("r11", Some("walking late"), "quartz"),
];

/// The second, folded write: some titles change.
const CHANGED: [(&str, Option<&str>, &str); 3] = [
    ("r01", Some("walk"), "quartz"),
    ("r05", Some("run reports"), "quartz"),
    ("r11", Some("run late"), "quartz"),
];

fn multi_doc((id, title, text): (&str, Option<&str>, &str)) -> Value {
    let mut attrs = json!({"text": text});
    if let Some(t) = title {
        attrs["title"] = json!(t);
    }
    json!({"id": id, "vector": [1.0, 0.5], "attributes": attrs})
}

/// The twin's row: `text` holds `multi`'s `title`, and `body` its `text`.
fn twin_doc((id, title, text): (&str, Option<&str>, &str)) -> Value {
    let mut attrs = json!({"body": text});
    if let Some(t) = title {
        attrs["text"] = json!(t);
    }
    json!({"id": id, "vector": [1.0, 0.5], "attributes": attrs})
}

async fn seed(a: &A, rows: &[(&str, Option<&str>, &str)], create: bool) {
    let (m, t) = if create {
        (
            json!({"schema": declared()}),
            json!({"schema": {"text": {"type": "string", "full_text_search": stemming()}}}),
        )
    } else {
        (json!({}), json!({}))
    };
    let mut m = m;
    m["documents"] = rows.iter().copied().map(multi_doc).collect();
    write(a, "multi", m).await;
    let mut t = t;
    t["documents"] = rows.iter().copied().map(twin_doc).collect();
    write(a, "twin", t).await;
}

/// Each filter over `title` on `multi`, its twin over `text`, and a row it must admit that
/// the primary's analyzer would refuse.
fn filters() -> Vec<(Value, Value, &'static str)> {
    let base = [
        (json!(["ContainsAllTokens", "Running reports"]), "r00"),
        (json!(["ContainsAnyToken", "Running"]), "r01"),
        (json!(["ContainsTokenSequence", "running late"]), "r02"),
        (
            json!(["ContainsAllTokens", "Running repo", {"last_as_prefix": true}]),
            "r03",
        ),
    ];
    let mut out = Vec::new();
    for (op, must) in base {
        let over = |attr: &str| {
            let mut f = vec![json!(attr)];
            f.extend(op.as_array().unwrap().iter().cloned());
            Value::Array(f)
        };
        let (m, t) = (over("title"), over("text"));
        out.push((m.clone(), t.clone(), must));
        out.push((json!(["Not", m.clone()]), json!(["Not", t.clone()]), ""));
        out.push((
            json!(["Or", [m, ["text", "ContainsAnyToken", "zebra"]]]),
            json!(["Or", [t, ["body", "ContainsAnyToken", "zebra"]]]),
            must,
        ));
    }
    out
}

/// The ids `f` admits on `index`, by an ordered query, a ranked one and a count, which agree.
async fn admitted(a: &A, index: &str, f: &Value, as_of: Option<&Value>) -> BTreeSet<String> {
    let at = |mut q: Value| {
        if let Some(e) = as_of {
            q["as_of"] = e.clone();
        }
        q
    };
    let ordered = ids_of(
        &query(
            a,
            index,
            &at(json!({"rank_by": ["id", "asc"], "top_k": 100, "filters": f})),
        )
        .await,
    );
    let ranked = ids_of(
        &query(
            a,
            index,
            &at(json!({"vector": [1.0, 0.5], "top_k": 100, "filters": f})),
        )
        .await,
    );
    assert_eq!(ordered, ranked, "{index} {f}: ordered against ranked");
    let b = query(
        a,
        index,
        &at(json!({"aggregate_by": {"n": ["Count", "id"]}, "filters": f})),
    )
    .await;
    assert_eq!(
        b["aggregations"]["n"].as_u64().unwrap(),
        ordered.len() as u64,
        "{index} {f}: counted {b}"
    );
    ordered
}

/// Every filter agrees with its twin at `point`; with `must`, each admits the row the
/// primary's analyzer would refuse, which holds while that row's title is `ROWS`'.
async fn every_filter_agrees(a: &A, as_of: Option<&Value>, point: &str, must: bool) {
    for (m, t, row) in filters() {
        let got = admitted(a, "multi", &m, as_of).await;
        let want = admitted(a, "twin", &t, as_of).await;
        assert_eq!(got, want, "{point}: {m}");
        let refuses_a_title = ROWS
            .iter()
            .any(|(id, title, _)| title.is_some() && !got.contains(*id));
        assert!(
            !got.is_empty() && refuses_a_title,
            "{point}: {m} admits {got:?}, which tells nothing"
        );
        if must && !row.is_empty() {
            assert!(got.contains(row), "{point}: {m} refused {row}: {got:?}");
        }
    }
}

#[tokio::test]
async fn a_further_fields_token_filter_is_analyzed_as_its_own_index() {
    // Criterion 1.
    let a = api();
    seed(&a, &ROWS, true).await;
    every_filter_agrees(&a, None, "before the first fold", true).await;
    fold(&a).await;
    every_filter_agrees(&a, None, "after the fold", true).await;
    let (s, b) = send(&a, "GET", "/v1/indexes/multi", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let first = b["updated_epoch"].clone();
    let (s, b) = send(&a, "GET", "/v1/indexes/twin", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["updated_epoch"], first, "one fold made both");
    seed(&a, &CHANGED, false).await;
    fold(&a).await;
    every_filter_agrees(&a, None, "after a second fold", false).await;
    every_filter_agrees(&a, Some(&first), "as_of the first fold", true).await;
    // The second fold changed what the live answer admits, so `as_of` is pinned.
    let (m, _, _) = &filters()[3];
    assert_ne!(
        admitted(&a, "multi", m, None).await,
        admitted(&a, "multi", m, Some(&first)).await,
        "{m}"
    );
}

#[tokio::test]
async fn writes_judge_a_further_field_by_its_own_analyzer() {
    // Criterion 2: in the creating fold, then against the HEAD.
    for folded in [false, true] {
        let a = api();
        let rows = [
            ("w1", Some("run"), "zebra"),
            ("w2", Some("running"), "zebra"),
            ("w3", Some("walk"), "zebra"),
            ("w4", Some("run fast"), "zebra"),
            ("w5", None, "zebra"),
        ];
        let docs: Vec<Value> = rows.iter().copied().map(multi_doc).collect();
        write(
            &a,
            "multi",
            json!({"schema": declared(), "documents": docs}),
        )
        .await;
        if folded {
            fold(&a).await;
        }
        let run = json!(["title", "ContainsAnyToken", "Running"]);
        write(
            &a,
            "multi",
            json!({"patch_by_filter": {"filters": run, "attributes": {"m": 1}}}),
        )
        .await;
        fold(&a).await;
        let all = json!({"rank_by": ["id", "asc"], "top_k": 100, "include_attributes": true});
        let b = query(&a, "multi", &all).await;
        let marked: BTreeSet<&str> = b["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["attributes"]["m"] == 1)
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            marked,
            BTreeSet::from(["w1", "w2", "w4"]),
            "folded {folded}: {b}"
        );
        // A conditional upsert, judged by `title`'s stemming.
        let cond = json!(["title", "ContainsAllTokens", "running"]);
        for id in ["w1", "w3"] {
            write(
                &a,
                "multi",
                json!({"documents": [multi_doc((id, Some("replaced"), "zebra"))],
                       "upsert_condition": cond}),
            )
            .await;
        }
        fold(&a).await;
        let b = query(
            &a,
            "multi",
            &json!({"rank_by": ["id", "asc"], "top_k": 100,
                    "filters": ["title", "ContainsAnyToken", "replaced"]}),
        )
        .await;
        assert_eq!(
            ids_of(&b),
            BTreeSet::from(["w1".into()]),
            "folded {folded}: {b}"
        );
        write(&a, "multi", json!({"delete_by_filter": run})).await;
        fold(&a).await;
        assert_eq!(
            ids_of(&query(&a, "multi", &all).await),
            BTreeSet::from(["w1".into(), "w3".into(), "w5".into()]),
            "folded {folded}"
        );
    }
}

#[tokio::test]
async fn a_token_filter_over_the_primary_is_unchanged() {
    // Criterion 4: against an index declaring the same primary and no further field.
    let a = api();
    let docs = |with_title: bool| -> Vec<Value> {
        ROWS.iter()
            .map(|r| {
                let mut d = multi_doc(*r);
                if !with_title {
                    d["attributes"].as_object_mut().unwrap().remove("title");
                }
                d
            })
            .collect()
    };
    write(
        &a,
        "multi",
        json!({"schema": declared(), "documents": docs(true)}),
    )
    .await;
    write(
        &a,
        "plain",
        json!({"schema": {"text": {"type": "string", "full_text_search": true}},
               "documents": docs(false)}),
    )
    .await;
    for folded in [false, true] {
        if folded {
            fold(&a).await;
        }
        for f in [
            json!(["text", "ContainsAnyToken", "zebra"]),
            json!(["text", "ContainsAllTokens", "quartz"]),
            json!(["Not", ["text", "ContainsAnyToken", "zebra"]]),
        ] {
            assert_eq!(
                admitted(&a, "multi", &f, None).await,
                admitted(&a, "plain", &f, None).await,
                "{f}, folded {folded}"
            );
        }
    }
}
