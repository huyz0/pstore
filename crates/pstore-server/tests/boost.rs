//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M62: a ranked query's window, rescored by its candidates' attributes.
//!
//! Every boosted answer is checked against the test's own model: the same query unboosted at
//! `top_k = window`, whose scores and attributes the model adds the terms to, in `f64` and in
//! term order, as the spec says.

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
        .header("x-pstore-tenant", "21")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn api() -> Arc<Api<MemoryStore>> {
    Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap()
}

async fn query<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>, body: &Value) -> Value {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
    b
}

async fn fold<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

const DAY: i64 = 86_400;

/// `2026-10-01T00:00:00Z`, the decays' origin, in seconds.
const ORIGIN: i64 = 1_790_812_800;

fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(DAY);
    let rem = secs.rem_euclid(DAY);
    // Civil from days (Howard Hinnant's algorithm), for a fixture date only.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn doc(i: usize) -> Value {
    let x = i as f32;
    let mut attrs = json!({
        "popularity": (i * 37 % 101) as i64,
        "price": f64::from(i as u32 % 13) * 2.5,
        "published": rfc3339(ORIGIN - (i as i64 % 9) * DAY / 2),
        "in_stock": !i.is_multiple_of(3),
        "tags": [format!("t{}", i % 4), "all"],
        "brand": format!("b{}", i % 5),
    });
    // Some documents lack attributes, and one holds another type.
    if i.is_multiple_of(11) {
        attrs.as_object_mut().unwrap().remove("popularity");
    }
    if i == 7 {
        attrs["popularity"] = json!("many");
    }
    json!({
        "id": format!("d{i:02}"),
        "vector": [x.cos(), x.sin(), 1.0, 0.5],
        "text": format!("common word{} word{}", i % 4, i % 7),
        "attributes": attrs,
    })
}

async fn write<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>, rows: std::ops::Range<usize>) {
    let docs: Vec<Value> = rows.map(doc).collect();
    let (s, b) = send(
        api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": docs,
                     "schema": {"published": "datetime"}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn terms() -> Value {
    json!([
        {"attr": "popularity", "fn": "saturate", "weight": 0.02, "pivot": 50},
        {"attr": "published", "fn": "decay", "weight": 0.03, "origin": rfc3339(ORIGIN),
         "scale": 2 * DAY, "shape": "gauss"},
        {"attr": "price", "fn": "linear", "weight": -0.0005},
        {"attr": "in_stock", "fn": "eq", "weight": 0.01, "value": true},
        {"attr": "tags", "fn": "eq", "weight": 0.004, "value": "t1"},
    ])
}

/// The datetime attribute, in seconds, as the fixture wrote it.
fn secs(v: &Value) -> Option<f64> {
    let s = v.as_str()?;
    let (date, time) = s.trim_end_matches('Z').split_once('T')?;
    let d: Vec<i64> = date.split('-').map(|p| p.parse().unwrap()).collect();
    let t: Vec<i64> = time.split(':').map(|p| p.parse().unwrap()).collect();
    // Days from civil (the inverse of `rfc3339`).
    let (y, m) = if d[1] <= 2 {
        (d[0] - 1, d[1] + 9)
    } else {
        (d[0], d[1] - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d[2] - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * DAY + t[0] * 3600 + t[1] * 60 + t[2]) as f64)
}

/// Rule 2's final score for one candidate: its score, then each term added in order.
fn boosted(base: f32, attrs: &Value) -> f32 {
    let num = |k: &str| match &attrs[k] {
        Value::Number(n) => n.as_f64(),
        _ => None,
    };
    let mut f = f64::from(base);
    if let Some(x) = num("popularity") {
        let x = x.max(0.0);
        f += 0.02 * (x / (x + 50.0));
    }
    if let Some(t) = secs(&attrs["published"]) {
        let d = (t - ORIGIN as f64).abs() / (2 * DAY) as f64;
        f += 0.03 * 0.5f64.powf(d * d);
    }
    if let Some(x) = num("price") {
        f += -0.0005 * x;
    }
    if attrs["in_stock"] == json!(true) {
        f += 0.01;
    }
    if attrs["tags"]
        .as_array()
        .is_some_and(|a| a.contains(&json!("t1")))
    {
        f += 0.004;
    }
    f as f32
}

/// The boosted answer the model predicts: `query` unboosted at `top_k = window`, rescored.
async fn model<S: BlobStore + Clone + 'static>(
    api: &Arc<Api<S>>,
    q: &Value,
    window: usize,
    top_k: usize,
) -> Vec<(String, u32)> {
    let mut plain = q.clone();
    plain["top_k"] = json!(window);
    plain["include_attributes"] = json!(true);
    let b = query(api, &plain).await;
    let mut scored: Vec<(usize, String, f32)> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(rank, r)| {
            // Term by term, in order, as rule 2 sums them.
            let base = r["score"].as_f64().unwrap() as f32;
            let total = boosted(base, &r["attributes"]);
            (rank, r["id"].as_str().unwrap().to_owned(), total)
        })
        .collect();
    scored.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)));
    scored
        .into_iter()
        .take(top_k)
        .map(|(_, id, s)| (id, s.to_bits()))
        .collect()
}

fn got(b: &Value) -> Vec<(String, u32)> {
    b["results"]
        .as_array()
        .unwrap_or_else(|| panic!("{b}"))
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                (r["score"].as_f64().unwrap() as f32).to_bits(),
            )
        })
        .collect()
}

#[tokio::test]
async fn boosts_rescore_the_window() {
    let api = api();
    write(&api, 0..30).await;
    let queries = [
        json!({"text": "word1 word3", "top_k": 5}),
        json!({"text": "word2", "vector": [0.3, 0.9, -0.2, 1.0], "exact": true, "top_k": 5}),
    ];
    for when in ["unfolded", "folded"] {
        for q in &queries {
            let want = model(&api, q, 20, 5).await;
            let mut boosted = q.clone();
            boosted["boost"] = json!({"window": 20, "terms": terms()});
            let b = query(&api, &boosted).await;
            assert_eq!(got(&b), want, "{when}: {q}");
            // The boost changed the answer: some document came from below the top 5.
            let plain = query(&api, q).await;
            let plain: Vec<String> = got(&plain).into_iter().map(|(id, _)| id).collect();
            assert!(
                want.iter().any(|(id, _)| !plain.contains(id)),
                "{when}: the fixture's boost lifts nothing: {q}"
            );
        }
        if when == "unfolded" {
            fold(&api).await;
            write(&api, 30..45).await;
            fold(&api).await;
        }
    }
    // Code review: `meta.unfolded_hits` on a boosted answer counts its returned rows that are
    // unfolded, which the boost reorders. Folded rows, and new ones held unfolded.
    write(&api, 45..60).await;
    let unfolded: Vec<String> = (45..60).map(|i| format!("d{i:02}")).collect();
    let mut differ = false;
    let mut some = false;
    for q in &queries {
        let want = model(&api, q, 20, 5).await;
        let mut boosted = q.clone();
        boosted["boost"] = json!({"window": 20, "terms": terms()});
        let b = query(&api, &boosted).await;
        assert_eq!(got(&b), want, "mixed: {q}");
        let fresh = want.iter().filter(|(id, _)| unfolded.contains(id)).count();
        assert_eq!(b["meta"]["unfolded_hits"], json!(fresh), "mixed: {q}: {b}");
        let plain = query(&api, q).await;
        differ |= plain["meta"]["unfolded_hits"] != json!(fresh);
        some |= fresh > 0;
    }
    assert!(
        differ,
        "the fixture's boost never changes how many unfolded rows are returned"
    );
    // And returns some (code review): a count stuck at 0 would pass the line above.
    assert!(some, "the fixture's boosted answers return no unfolded row");
    let q = &queries[0];
    let mut boosted = q.clone();
    boosted["boost"] = json!({"window": 20, "terms": terms()});
    let want = model(&api, q, 20, 5).await;
    // The boost reads every attribute, whatever the answer returns.
    for include in [json!(["brand"]), json!(false)] {
        let mut b = boosted.clone();
        b["include_attributes"] = include.clone();
        assert_eq!(
            got(&query(&api, &b).await),
            want,
            "include_attributes {include}"
        );
    }
    // A multi-query: each sub-query its own boost, and one without none.
    let mut other = queries[1].clone();
    other["boost"] = json!({"window": 12, "terms": terms()});
    let multi = json!({"queries": [boosted.clone(), queries[1].clone(), other.clone()]});
    let b = query(&api, &multi).await;
    let subs = b["results"].as_array().unwrap_or_else(|| panic!("{b}"));
    assert_eq!(got(&json!({"results": subs[0]})), want, "sub-query 0");
    assert_eq!(
        got(&json!({"results": subs[1]})),
        got(&query(&api, &queries[1]).await),
        "sub-query 1, unboosted"
    );
    assert_eq!(
        got(&json!({"results": subs[2]})),
        model(&api, &queries[1], 12, 5).await,
        "sub-query 2"
    );
    // `as_of` the first fold's epoch: that epoch's window, rescored.
    let mut then = q.clone();
    then["as_of"] = json!(1);
    let want = model(&api, &then, 20, 5).await;
    let mut boosted = then.clone();
    boosted["boost"] = json!({"window": 20, "terms": terms()});
    assert_eq!(got(&query(&api, &boosted).await), want, "as_of");
}

/// `id`'s final score under `terms`, and its unboosted score, for a query admitting only it.
async fn one(api: &Arc<Api<MemoryStore>>, id: &str, terms: Value) -> (f32, f32) {
    let q = json!({"text": "common", "top_k": 1, "filters": ["id", "Eq", id]});
    let base = query(api, &q).await["results"][0]["score"]
        .as_f64()
        .unwrap() as f32;
    let mut boosted = q.clone();
    boosted["boost"] = json!({"terms": terms});
    let b = query(api, &boosted).await;
    (b["results"][0]["score"].as_f64().unwrap() as f32, base)
}

#[tokio::test]
async fn each_boost_function_scores_as_specified() {
    let api = api();
    let docs = vec![
        json!({"id": "a", "vector": [1.0, 0.0], "text": "common",
               "attributes": {"n": 4, "f": 2.5, "neg": -3, "when": "2026-10-02T00:00:00Z",
                              "s": "red", "b": true, "arr": ["x", "y"], "x": 1.5,
                              "huge": 1e300}}),
        json!({"id": "bare", "vector": [0.0, 1.0], "text": "common",
               "attributes": {"n": "four"}}),
    ];
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"documents": docs, "schema": {"when": "datetime"}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // (terms, the boost by hand)
    let cases = [
        (json!([{"attr": "n", "fn": "linear", "weight": 0.5}]), 2.0),
        (json!([{"attr": "f", "fn": "linear", "weight": 2.0}]), 5.0),
        (
            json!([{"attr": "n", "fn": "saturate", "weight": 1.0, "pivot": 4}]),
            0.5,
        ),
        (
            json!([{"attr": "neg", "fn": "saturate", "weight": 1.0, "pivot": 4}]),
            0.0,
        ),
        (
            json!([{"attr": "n", "fn": "decay", "weight": 1.0, "origin": 0, "scale": 2}]),
            0.25,
        ),
        (
            json!([{"attr": "n", "fn": "decay", "weight": 1.0, "origin": 0, "scale": 2,
                 "shape": "gauss"}]),
            0.0625,
        ),
        (
            json!([{"attr": "n", "fn": "decay", "weight": 1.0, "origin": 2, "scale": 2,
                 "shape": "linear"}]),
            0.5,
        ),
        (
            json!([{"attr": "n", "fn": "decay", "weight": 1.0, "origin": 20, "scale": 2,
                 "shape": "linear"}]),
            0.0,
        ),
        (
            json!([{"attr": "f", "fn": "decay", "weight": 1.0, "origin": 2.5, "scale": 1}]),
            1.0,
        ),
        // A day from the origin, at a scale of a day.
        (
            json!([{"attr": "when", "fn": "decay", "weight": 1.0,
                 "origin": "2026-10-01T00:00:00Z", "scale": 86_400}]),
            0.5,
        ),
        (
            json!([{"attr": "s", "fn": "eq", "weight": 1.0, "value": "red"}]),
            1.0,
        ),
        (
            json!([{"attr": "s", "fn": "eq", "weight": 1.0, "value": "blue"}]),
            0.0,
        ),
        (
            json!([{"attr": "b", "fn": "eq", "weight": 1.0, "value": true}]),
            1.0,
        ),
        (
            json!([{"attr": "x", "fn": "eq", "weight": 1.0, "value": 1.5}]),
            1.0,
        ),
        (
            json!([{"attr": "arr", "fn": "eq", "weight": 1.0, "value": "y"}]),
            1.0,
        ),
        (
            json!([{"attr": "missing", "fn": "linear", "weight": 1.0}]),
            0.0,
        ),
        // An int against a float value, exactly; a datetime against the instant a string is.
        (
            json!([{"attr": "n", "fn": "eq", "weight": 1.0, "value": 4.0}]),
            1.0,
        ),
        (
            json!([{"attr": "when", "fn": "eq", "weight": 1.0,
                 "value": "2026-10-02T00:00:00Z"}]),
            1.0,
        ),
        (
            json!([{"attr": "when", "fn": "eq", "weight": 1.0,
                 "value": "2026-10-03T00:00:00Z"}]),
            0.0,
        ),
        // Types a term does not read contribute nothing.
        (
            json!([{"attr": "when", "fn": "decay", "weight": 1.0, "origin": 0, "scale": 1}]),
            0.0,
        ),
        (
            json!([{"attr": "n", "fn": "decay", "weight": 1.0,
                 "origin": "2026-10-01T00:00:00Z", "scale": 1}]),
            0.0,
        ),
        (json!([{"attr": "arr", "fn": "linear", "weight": 1.0}]), 0.0),
        (
            json!([{"attr": "s", "fn": "saturate", "weight": 1.0, "pivot": 1}]),
            0.0,
        ),
        (
            json!([{"attr": "when", "fn": "decay", "weight": 1.0,
                 "origin": "2026-10-01T00:00:00Z", "scale": 43_200, "shape": "gauss"}]),
            0.0625,
        ),
        (
            json!([{"attr": "when", "fn": "decay", "weight": 1.0,
                 "origin": "2026-10-01T00:00:00Z", "scale": 86_400, "shape": "linear"}]),
            0.5,
        ),
        (
            json!([{"attr": "f", "fn": "decay", "weight": 1.0, "origin": 0.5, "scale": 1,
                 "shape": "gauss"}]),
            0.0625,
        ),
        // Terms sum, in order.
        (
            json!([{"attr": "n", "fn": "linear", "weight": 0.5},
                {"attr": "s", "fn": "eq", "weight": 0.25, "value": "red"}]),
            2.25,
        ),
    ];
    for (terms, by_hand) in cases {
        let (score, base) = one(&api, "a", terms.clone()).await;
        assert_eq!(
            score,
            (f64::from(base) + by_hand) as f32,
            "{terms}: base {base}"
        );
    }
    // A term that overflows contributes nothing; a finite final beyond f32 is clamped.
    let (score, base) = one(
        &api,
        "a",
        json!([{"attr": "huge", "fn": "linear", "weight": 1e300},
               {"attr": "n", "fn": "linear", "weight": 0.5}]),
    )
    .await;
    assert_eq!(score, (f64::from(base) + 2.0) as f32, "overflowing term");
    let (score, _) = one(
        &api,
        "a",
        json!([{"attr": "huge", "fn": "linear", "weight": 1.0}]),
    )
    .await;
    assert_eq!(score, f32::MAX, "clamped");
    let (score, _) = one(
        &api,
        "a",
        json!([{"attr": "huge", "fn": "linear", "weight": -1.0}]),
    )
    .await;
    assert_eq!(score, -f32::MAX, "clamped below");
    // A string where `linear` wants a number contributes nothing, as a missing one does.
    let (score, base) = one(
        &api,
        "bare",
        json!([{"attr": "n", "fn": "linear", "weight": 1.0}]),
    )
    .await;
    assert_eq!(score, base);
}

#[tokio::test]
async fn the_window_bounds_what_a_boost_can_lift() {
    let api = api();
    write(&api, 0..30).await;
    let q = json!({"text": "common word1", "top_k": 3});
    let plain = query(&api, &json!({"text": "common word1", "top_k": 30})).await;
    let order: Vec<String> = got(&plain).into_iter().map(|(id, _)| id).collect();
    // The sixth: outside the top 3, inside a window of 10. A boost only it can take.
    let target = &order[5];
    let lift = json!([{"attr": "id_copy", "fn": "eq", "weight": 100.0, "value": target}]);
    // `id` is not an attribute, so the fixture carries a copy.
    let docs: Vec<Value> = (0..30)
        .map(|i| {
            let mut d = doc(i);
            d["attributes"]["id_copy"] = d["id"].clone();
            d
        })
        .collect();
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"documents": docs, "schema": {"published": "datetime"}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut wide = q.clone();
    wide["boost"] = json!({"window": 10, "terms": lift});
    let b = query(&api, &wide).await;
    assert_eq!(got(&b)[0].0, *target, "lifted from inside the window: {b}");
    let mut narrow = q.clone();
    narrow["boost"] = json!({"window": 3, "terms": lift});
    let b = query(&api, &narrow).await;
    assert!(
        got(&b).iter().all(|(id, _)| id != target),
        "lifted from outside the window: {b}"
    );
    // A dense leg alone: the sixth is one the leg's own cut at 3 would have dropped.
    let v = json!({"vector": [0.3, 0.9, -0.2, 1.0], "exact": true, "top_k": 3});
    let mut all = v.clone();
    all["top_k"] = json!(30);
    let order: Vec<String> = got(&query(&api, &all).await)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let target = &order[5];
    let mut wide = v.clone();
    wide["boost"] = json!({"window": 10, "terms": [{"attr": "id_copy", "fn": "eq",
                                                   "weight": 100.0, "value": target}]});
    let b = query(&api, &wide).await;
    assert_eq!(
        got(&b)[0].0,
        *target,
        "a dense leg widened to the window: {b}"
    );
}

#[tokio::test]
async fn boosts_are_refused_at_the_door() {
    let api = api();
    write(&api, 0..10).await;
    let t = json!({"attr": "price", "fn": "linear", "weight": 1.0});
    let q = |boost: Value| json!({"text": "common", "top_k": 5, "boost": boost});
    let nine: Vec<Value> = (0..9).map(|_| t.clone()).collect();
    let cases = [
        ("window below top_k", q(json!({"window": 4, "terms": [t]}))),
        (
            "window above 1,000",
            q(json!({"window": 1001, "terms": [t]})),
        ),
        ("no terms", q(json!({"terms": []}))),
        ("nine terms", q(json!({"terms": nine}))),
        (
            "an unknown fn",
            q(json!({"terms": [{"attr": "price", "fn": "log", "weight": 1.0}]})),
        ),
        (
            "a field fn does not take",
            q(json!({"terms": [{"attr": "price", "fn": "linear",
            "weight": 1.0, "pivot": 3}]})),
        ),
        ("an unknown key", q(json!({"terms": [t], "k": 1}))),
        (
            "no weight",
            q(json!({"terms": [{"attr": "price", "fn": "linear"}]})),
        ),
        (
            "a string weight",
            q(json!({"terms": [{"attr": "price", "fn": "linear", "weight": "1"}]})),
        ),
        (
            "pivot 0",
            q(
                json!({"terms": [{"attr": "price", "fn": "saturate", "weight": 1.0,
            "pivot": 0}]}),
            ),
        ),
        (
            "no pivot",
            q(json!({"terms": [{"attr": "price", "fn": "saturate", "weight": 1.0}]})),
        ),
        (
            "scale 0",
            q(
                json!({"terms": [{"attr": "price", "fn": "decay", "weight": 1.0,
            "origin": 0, "scale": 0}]}),
            ),
        ),
        (
            "no origin",
            q(
                json!({"terms": [{"attr": "price", "fn": "decay", "weight": 1.0,
            "scale": 1}]}),
            ),
        ),
        (
            "a bad origin",
            q(
                json!({"terms": [{"attr": "price", "fn": "decay", "weight": 1.0,
            "origin": "yesterday", "scale": 1}]}),
            ),
        ),
        (
            "an unknown shape",
            q(
                json!({"terms": [{"attr": "price", "fn": "decay", "weight": 1.0,
            "origin": 0, "scale": 1, "shape": "cosine"}]}),
            ),
        ),
        (
            "an array value",
            q(json!({"terms": [{"attr": "tags", "fn": "eq", "weight": 1.0,
            "value": ["a"]}]})),
        ),
        (
            "no value",
            q(json!({"terms": [{"attr": "tags", "fn": "eq", "weight": 1.0}]})),
        ),
        (
            "an empty attr",
            q(json!({"terms": [{"attr": "", "fn": "linear", "weight": 1.0}]})),
        ),
        (
            "id",
            q(json!({"terms": [{"attr": "id", "fn": "linear", "weight": 1.0}]})),
        ),
        (
            "a $ attr",
            q(json!({"terms": [{"attr": "$dist", "fn": "linear", "weight": 1.0}]})),
        ),
        (
            "a fractional window",
            q(json!({"window": 10.5, "terms": [t]})),
        ),
        (
            "top_k above 1,000",
            json!({"text": "common", "top_k": 1001,
            "boost": {"terms": [t]}}),
        ),
        ("boost not an object", q(json!([t]))),
        (
            "no attr",
            q(json!({"terms": [{"fn": "linear", "weight": 1.0}]})),
        ),
        (
            "no fn",
            q(json!({"terms": [{"attr": "price", "weight": 1.0}]})),
        ),
        (
            "no scale",
            q(
                json!({"terms": [{"attr": "price", "fn": "decay", "weight": 1.0,
            "origin": 0}]}),
            ),
        ),
        (
            "a null weight",
            q(json!({"terms": [{"attr": "price", "fn": "linear",
            "weight": null}]})),
        ),
        (
            "beside rank_by",
            json!({"rank_by": ["price", "asc"], "top_k": 5,
            "boost": {"terms": [t]}}),
        ),
        (
            "beside aggregate_by",
            json!({"aggregate_by": {"c": ["Count", "id"]},
            "boost": {"terms": [t]}}),
        ),
    ];
    for (what, body) in &cases {
        let (s, b) = send(&api, "POST", "/v1/indexes/docs/query", Some(body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{what}: {b}");
    }
    // The limits themselves: a window of 1,000 and of top_k, and 8 terms.
    let eight: Vec<Value> = (0..8).map(|_| t.clone()).collect();
    for boost in [
        json!({"window": 1000, "terms": eight}),
        json!({"window": 5, "terms": [t]}),
    ] {
        let (s, b) = send(&api, "POST", "/v1/indexes/docs/query", Some(&q(boost))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
}

#[tokio::test]
async fn a_boost_costs_its_window() {
    use pstore_testkit::depth::DepthCounting;
    // ⚠️ At one segment as well as four: a chain in one segment's task hides behind another's
    // reads overlapping it (M61's finding). And with unfolded rows: the shadow check changes
    // which rows the row round reads.
    for (segments, unfolded) in [(1, false), (4, false), (4, true)] {
        let store = DepthCounting::new(MemoryStore::new());
        let cold = || Api::new(Accounted::new(store.clone()), LaneId(1)).unwrap();
        let writer = cold();
        for k in 0..segments {
            write(&writer, k * 10..k * 10 + 10).await;
            fold(&writer).await;
        }
        let mut seen = Vec::new();
        for body in [
            json!({"text": "common word2", "top_k": 30}),
            json!({"text": "common word2", "top_k": 5, "boost": {"window": 30, "terms": terms()}}),
        ] {
            let api = cold();
            if unfolded {
                // Rewrites of folded ids, held unfolded by the querying process.
                write(&api, 3..8).await;
            }
            store.reset();
            let b = query(&api, &body).await;
            assert!(!b["results"].as_array().unwrap().is_empty(), "{b}");
            let cost = &b["meta"]["cost"];
            seen.push((
                store.depth(),
                cost["blob_reads"].clone(),
                cost["bytes_read"].clone(),
            ));
        }
        assert_eq!(
            seen[0].0, 4,
            "{segments} segment(s), unfolded {unfolded}: {seen:?}"
        );
        assert_eq!(
            seen[0], seen[1],
            "{segments} segment(s), unfolded {unfolded}: unboosted at the window, then boosted"
        );
    }
}

#[tokio::test]
async fn the_default_window_is_twice_top_k_and_at_least_100() {
    // `min(1000, max(100, 2·top_k))`: at top_k 10 the window is 100, at 60 it is 120.
    let api = api();
    let docs: Vec<Value> = (0..140)
        .map(|i| {
            let x = i as f32 * 0.01;
            json!({"id": format!("d{i:03}"), "vector": [1.0, x], "text": "common",
                   "attributes": {"id_copy": format!("d{i:03}")}})
        })
        .collect();
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"documents": docs})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let v = json!({"vector": [1.0, 0.0], "exact": true, "top_k": 140});
    let order: Vec<String> = got(&query(&api, &v).await)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    for (top_k, inside, outside) in [(10, 99, 100), (60, 119, 120)] {
        for (rank, lifted) in [(inside, true), (outside, false)] {
            let target = &order[rank];
            let b = query(
                &api,
                &json!({"vector": [1.0, 0.0], "exact": true, "top_k": top_k,
                        "boost": {"terms": [{"attr": "id_copy", "fn": "eq", "weight": 100.0,
                                             "value": target}]}}),
            )
            .await;
            assert_eq!(
                got(&b)[0].0 == *target,
                lifted,
                "top_k {top_k}: rank {rank} lifted {lifted}: {b}"
            );
        }
    }
}
