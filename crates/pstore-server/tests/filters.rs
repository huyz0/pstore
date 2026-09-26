//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Filters through the API — M9b. A query answers only the documents a predicate admits,
//! and filters before the limit, never after it.

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

fn api() -> Arc<Api<MemoryStore>> {
    Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).expect("a fencing backend")
}

async fn send(api: &Arc<Api<MemoryStore>>, req: Request<Body>) -> (StatusCode, Value) {
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn request(method: &str, uri: &str, body: Option<&Value>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "11")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

/// Ten documents: `n` = i, `lang` alternating en/fr/de, `odd` only on odd i, and prose.
fn corpus() -> Value {
    let langs = ["en", "fr", "de"];
    let documents: Vec<Value> = (0..10i64)
        .map(|i| {
            let mut attrs = json!({"n": i, "lang": langs[i as usize % 3]});
            if i % 2 == 1 {
                attrs["odd"] = json!("yes");
            }
            json!({
                "id": format!("d{i}"),
                "vector": [1.0, i as f32 / 10.0],
                "text": if i < 5 { "apple pie recipe" } else { "apple orchard news" },
                "attributes": attrs
            })
        })
        .collect();
    json!({"durability": "durable", "documents": documents})
}

async fn seeded(fold: bool) -> Arc<Api<MemoryStore>> {
    let api = api();
    let (s, b) = send(
        &api,
        request("PUT", "/v1/indexes/docs/documents", Some(&corpus())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    if fold {
        let (s, b) = send(&api, request("POST", "/v1/admin/fold", None)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    api
}

async fn ids(api: &Arc<Api<MemoryStore>>, q: &Value) -> BTreeSet<String> {
    let (s, b) = send(api, request("POST", "/v1/indexes/docs/query", Some(q))).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn set(ns: &[i64]) -> BTreeSet<String> {
    ns.iter().map(|n| format!("d{n}")).collect()
}

fn filtered(f: Value) -> Value {
    json!({"vector": [1.0, 0.5], "top_k": 100, "filters": f})
}

#[tokio::test]
async fn every_operator_admits_exactly_its_documents() {
    for fold in [false, true] {
        let api = seeded(fold).await;
        for (f, want) in [
            (json!(["n", "Eq", 3]), set(&[3])),
            (json!(["lang", "Eq", "fr"]), set(&[1, 4, 7])),
            (json!(["lang", "NotEq", "fr"]), set(&[0, 2, 3, 5, 6, 8, 9])),
            (json!(["n", "In", [2, 5, 99]]), set(&[2, 5])),
            (json!(["lang", "NotIn", ["en", "de"]]), set(&[1, 4, 7])),
            (json!(["n", "Lt", 3]), set(&[0, 1, 2])),
            (json!(["n", "Lte", 3]), set(&[0, 1, 2, 3])),
            (json!(["n", "Gt", 7]), set(&[8, 9])),
            (json!(["n", "Gte", 7]), set(&[7, 8, 9])),
            // Strings by bytes: "de" < "en" < "fr".
            (json!(["lang", "Lt", "en"]), set(&[2, 5, 8])),
            (json!(["lang", "Gte", "en"]), set(&[0, 1, 3, 4, 6, 7, 9])),
            // A type mismatch is false, not an error and not a coercion.
            (json!(["n", "Gt", "5"]), set(&[])),
            (json!(["n", "Lt", "5"]), set(&[])),
            (json!(["odd", "Eq", null]), set(&[0, 2, 4, 6, 8])),
            (json!(["odd", "NotEq", null]), set(&[1, 3, 5, 7, 9])),
            // NotEq admits documents that lack the attribute.
            (json!(["odd", "NotEq", "yes"]), set(&[0, 2, 4, 6, 8])),
            (json!(["id", "Eq", "d6"]), set(&[6])),
            (json!(["id", "In", ["d1", "d9", "zz"]]), set(&[1, 9])),
            (
                json!([
                    "And",
                    [["n", "Gte", 2], ["n", "Lt", 6], ["lang", "NotEq", "de"]]
                ]),
                set(&[3, 4]),
            ),
            (
                json!(["Or", [["n", "Eq", 0], ["lang", "Eq", "de"]]]),
                set(&[0, 2, 5, 8]),
            ),
            (json!(["Not", ["n", "Lt", 8]]), set(&[8, 9])),
            (json!(["And", []]), set(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9])),
            (json!(["Or", []]), set(&[])),
        ] {
            assert_eq!(
                ids(&api, &filtered(f.clone())).await,
                want,
                "fold={fold}: {f}"
            );
        }
    }
}

#[tokio::test]
async fn a_filter_restricts_text_and_hybrid_legs_too() {
    let api = seeded(true).await;
    let text = json!({"text": "apple", "top_k": 100, "filters": ["n", "Gte", 8]});
    assert_eq!(ids(&api, &text).await, set(&[8, 9]));
    let hybrid = json!({"text": "recipe", "vector": [1.0, 0.0], "top_k": 100,
                        "filters": ["lang", "Eq", "en"]});
    assert_eq!(ids(&api, &hybrid).await, set(&[0, 3, 6, 9]));
}

#[tokio::test]
async fn filtering_happens_before_the_limit() {
    // The two nearest documents to [1.0, 0.0] are d0 and d1; the filter admits neither. A
    // post-filter over the top 2 would answer with nothing.
    let api = seeded(true).await;
    let q = json!({"vector": [1.0, 0.0], "top_k": 2, "filters": ["n", "Gte", 8]});
    assert_eq!(ids(&api, &q).await, set(&[8, 9]));
}

#[tokio::test]
async fn a_filtered_answer_is_the_unfiltered_one_restricted() {
    let api = seeded(true).await;
    let all = json!({"vector": [1.0, 0.3], "top_k": 100});
    let (_, unfiltered) = send(&api, request("POST", "/v1/indexes/docs/query", Some(&all))).await;
    let mut q = all.clone();
    q["filters"] = json!(["odd", "Eq", "yes"]);
    let (_, filtered) = send(&api, request("POST", "/v1/indexes/docs/query", Some(&q))).await;
    let odd: Vec<&Value> = unfiltered["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["id"].as_str().unwrap()[1..].parse::<i64>().unwrap() % 2 == 1)
        .map(|r| &r["id"])
        .collect();
    let got: Vec<&Value> = filtered["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["id"])
        .collect();
    assert_eq!(
        got, odd,
        "a filter changed the order among the documents it admits"
    );
}

#[tokio::test]
async fn a_malformed_filter_is_refused() {
    let api = seeded(false).await;
    for f in [
        json!(["n", "Like", 3]),
        json!(["n", "Eq"]),
        json!(["n", "Eq", 1.5]),
        json!(["n", "Eq", true]),
        json!(["n", "Lt", null]),
        json!(["n", "In", 3]),
        json!(["n", "In", [1, 2.5]]),
        json!(["And", ["n", "Eq", 1]]),
        json!(["Not", []]),
        json!("n"),
        json!([1, "Eq", 1]),
        json!(["n", "In", [1, null]]),
        json!(["Nope", ["n", "Eq", 1]]),
    ] {
        let (s, b) = send(
            &api,
            request("POST", "/v1/indexes/docs/query", Some(&filtered(f.clone()))),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{f} was accepted: {b}");
        assert_eq!(b["error"]["code"], "bad_request", "{f}");
    }
}

#[tokio::test]
async fn a_negation_keeps_the_rows_that_lack_the_attribute() {
    // One block holding `{n: 5}` and `{}`: its zone is `n: (5, 5)`, and `NotEq n 5` must
    // still read it for the row with no `n`.
    let api = api();
    let body = json!({"durability": "durable", "documents": [
        {"id": "has", "vector": [1.0, 0.0], "attributes": {"n": 5}},
        {"id": "lacks", "vector": [0.0, 1.0]}
    ]});
    send(
        &api,
        request("PUT", "/v1/indexes/docs/documents", Some(&body)),
    )
    .await;
    send(&api, request("POST", "/v1/admin/fold", None)).await;
    let got = ids(&api, &filtered(json!(["n", "NotEq", 5]))).await;
    assert_eq!(got, BTreeSet::from(["lacks".to_owned()]));
}

#[tokio::test]
async fn time_travel_takes_the_filter_too() {
    let api = seeded(true).await;
    let (_, head) = send(&api, request("GET", "/v1/indexes/docs", None)).await;
    let mut q = filtered(json!(["n", "Lt", 2]));
    q["as_of"] = head["epoch"].clone();
    assert_eq!(ids(&api, &q).await, set(&[0, 1]));
}

/// Eight documents built so both legs' orders are known. `d0`..`d5` are admitted by `n <= 5`;
/// `d6` and `d7` are excluded and are the best of BOTH legs, so a leg that stopped at its limit
/// and was masked afterwards answers with nothing. Among the admitted, the dense order is
/// d0 d1 d2 d3 d4 d5 (`[1, y]` against `[1, 1]`) and the text order d3 d4 d2 d5 d1 d0 ("apple"
/// repeated): d2 is third in both, which wins RRF only if a leg contributes past its limit.
async fn graded() -> Arc<Api<MemoryStore>> {
    let api = api();
    let apples = [1usize, 2, 4, 6, 5, 3, 10, 9];
    let y = [1.0f32, 0.8, 0.6, 0.4, 0.2, 0.0, 2.0, 1.5];
    let documents: Vec<Value> = (0..8usize)
        .map(|i| {
            json!({
                "id": format!("d{i}"),
                "vector": [1.0, y[i]],
                "text": format!("{} filler words here", "apple ".repeat(apples[i])),
                "attributes": {"n": i}
            })
        })
        .collect();
    let body = json!({"durability": "durable", "documents": documents});
    send(
        &api,
        request("PUT", "/v1/indexes/docs/documents", Some(&body)),
    )
    .await;
    send(&api, request("POST", "/v1/admin/fold", None)).await;
    api
}

/// The ids of a query's rows, in rank order.
async fn ranked(api: &Arc<Api<MemoryStore>>, q: &Value) -> Vec<String> {
    let (s, b) = send(api, request("POST", "/v1/indexes/docs/query", Some(q))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn each_leg_is_masked_before_its_own_limit() {
    // ⚠️ Found at code review: every other filtered test asks for more rows than exist, so a
    // text leg that stopped at its limit, or a limit not applied after the mask, passed them.
    let api = graded().await;
    let admit = json!(["n", "Lte", 5]);
    let text_all = ranked(
        &api,
        &json!({"text": "apple", "top_k": 100, "filters": admit}),
    )
    .await;
    let dense_all = ranked(
        &api,
        &json!({"vector": [1.0, 1.0], "top_k": 100, "filters": admit}),
    )
    .await;
    // The fixture's premise, asserted rather than assumed.
    assert_eq!(dense_all, ["d0", "d1", "d2", "d3", "d4", "d5"]);
    assert_eq!(text_all, ["d3", "d4", "d2", "d5", "d1", "d0"]);

    // One leg, `top_k` below the admitted count: the leg's first admitted row.
    let one = ranked(
        &api,
        &json!({"text": "apple", "top_k": 1, "filters": admit}),
    )
    .await;
    assert_eq!(one, text_all[..1]);

    // Two legs at `top_k = 2`: RRF (k = 60, 1-based ranks) over each leg's first TWO admitted
    // rows, ties broken by row -- which is the document number here.
    let limit = 2;
    let mut score: std::collections::BTreeMap<usize, f32> = std::collections::BTreeMap::new();
    for leg in [&dense_all, &text_all] {
        for (rank, id) in leg.iter().take(limit).enumerate() {
            let row: usize = id[1..].parse().unwrap();
            *score.entry(row).or_insert(0.0) += 1.0 / (60.0 + (rank + 1) as f32);
        }
    }
    let mut want: Vec<(usize, f32)> = score.into_iter().collect();
    want.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let want: Vec<String> = want
        .iter()
        .take(limit)
        .map(|(r, _)| format!("d{r}"))
        .collect();
    let q = json!({"text": "apple", "vector": [1.0, 1.0], "top_k": limit, "filters": admit});
    assert_eq!(ranked(&api, &q).await, want);
}
