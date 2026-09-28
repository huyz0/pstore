//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Aggregations (M12): `aggregate_by` and `group_by`, checked against a brute-force model of
//! every row written, upserted and deleted.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_testkit::depth::DepthCounting;
use pstore_types::LaneId;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

type Store = DepthCounting<MemoryStore>;
type A = Arc<Api<Store>>;

struct World {
    depth: Store,
    apis: Vec<A>,
}

fn world(lanes: &[u64]) -> World {
    let depth = DepthCounting::new(MemoryStore::new());
    let apis = lanes
        .iter()
        .map(|l| Api::new(Accounted::new(depth.clone()), LaneId(*l)).unwrap())
        .collect();
    World { depth, apis }
}

async fn send(
    api: &A,
    method: &str,
    uri: &str,
    body: &Value,
    token: Option<&str>,
) -> (StatusCode, Value, Option<String>) {
    let mut r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "31");
    if let Some(t) = token {
        r = r.header("x-pstore-session", t);
    }
    let res = Arc::clone(api)
        .router()
        .oneshot(r.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let token = res
        .headers()
        .get("x-pstore-session")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        token,
    )
}

/// The rows as the model holds them: id -> attributes.
type Model = BTreeMap<String, Map<String, Value>>;

async fn upsert(
    api: &A,
    model: &mut Model,
    rows: &[(String, Map<String, Value>)],
    durable: bool,
) -> String {
    let documents: Vec<Value> = rows
        .iter()
        .map(|(id, attrs)| json!({"id": id, "vector": [1.0, 0.5], "attributes": attrs}))
        .collect();
    let body =
        json!({"durability": if durable { "durable" } else { "batched" }, "documents": documents});
    let (s, b, t) = send(api, "PUT", "/v1/indexes/docs/documents", &body, None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for (id, attrs) in rows {
        model.insert(id.clone(), attrs.clone());
    }
    t.unwrap()
}

async fn delete(api: &A, model: &mut Model, ids: &[&str], durable: bool) {
    let body = json!({"durability": if durable { "durable" } else { "batched" }, "deletes": ids});
    let (s, b, _) = send(api, "PUT", "/v1/indexes/docs/documents", &body, None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for id in ids {
        model.remove(*id);
    }
}

async fn fold(api: &A) -> u64 {
    let (s, b, _) = send(api, "POST", "/v1/admin/fold", &json!({}), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["epoch"].as_u64().unwrap()
}

async fn query(api: &A, q: &Value) -> Value {
    let (s, b, _) = send(api, "POST", "/v1/indexes/docs/query", q, None).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b
}

const COLORS: [&str; 5] = ["red", "green", "blue", "black", "white"];

fn attrs(i: usize) -> Map<String, Value> {
    let mut a = Map::new();
    if !i.is_multiple_of(17) {
        a.insert("color".into(), json!(COLORS[i % 5]));
    }
    a.insert("price".into(), json!(i % 100 + 1));
    // Mixed: every third a float, all positive, so no sum cancels near zero.
    let w = if i.is_multiple_of(3) {
        json!(i as f64 + 0.25)
    } else {
        json!(i)
    };
    a.insert("w".into(), w);
    a.insert("even".into(), json!(i.is_multiple_of(2)));
    if i.is_multiple_of(7) {
        a.insert("label".into(), json!("s"));
    }
    a
}

fn rows(range: std::ops::Range<usize>) -> Vec<(String, Map<String, Value>)> {
    range.map(|i| (format!("d{i:05}"), attrs(i))).collect()
}

fn admits(filter: Option<i64>, a: &Map<String, Value>) -> bool {
    filter.is_none_or(|min| a["price"].as_i64().unwrap() > min)
}

/// What the aggregates must be, computed from the model.
fn expected(model: &Model, filter: Option<i64>) -> (u64, u64, i64, f64) {
    let mut n = 0;
    let mut colored = 0;
    let mut price = 0;
    let mut w = 0.0;
    for a in model.values().filter(|a| admits(filter, a)) {
        n += 1;
        colored += u64::from(a.contains_key("color"));
        price += a["price"].as_i64().unwrap();
        w += a["w"].as_f64().unwrap();
    }
    (n, colored, price, w)
}

fn aggregates(filter: Option<i64>) -> Value {
    let mut q = json!({"aggregate_by": {
        "n": ["Count", "id"],
        "colored": ["Count", "color"],
        "price": ["Sum", "price"],
        "w": ["Sum", "w"],
    }});
    if let Some(min) = filter {
        q["filters"] = json!(["price", "Gt", min]);
    }
    q
}

fn check(got: &Value, want: (u64, u64, i64, f64), what: &str) {
    let a = &got["aggregations"];
    assert_eq!(a["n"], want.0, "{what}: {got}");
    assert_eq!(a["colored"], want.1, "{what}: {got}");
    assert_eq!(a["price"], want.2, "{what}: {got}");
    let w = a["w"].as_f64().unwrap();
    assert!(
        (w - want.3).abs() <= want.3.abs() * 1e-9,
        "{what}: {w} against {}",
        want.3
    );
    assert_eq!(got["results"], json!([]));
}

/// 2,000 rows in 4 folds, a folded delete, then unfolded writes: an upsert of a folded id, a
/// delete of another, and a new row.
async fn seeded(a: &A) -> (Model, u64, Model) {
    let mut model = Model::new();
    let mut past = None;
    for k in 0..4 {
        upsert(a, &mut model, &rows(k * 500..(k + 1) * 500), true).await;
        let e = fold(a).await;
        if k == 1 {
            past = Some((e, model.clone()));
        }
    }
    delete(a, &mut model, &["d00003", "d01003"], true).await;
    fold(a).await;
    let mut up = attrs(5);
    up.insert("price".into(), json!(1000));
    up.insert("w".into(), json!(0.5));
    upsert(a, &mut model, &[("d00005".to_owned(), up)], false).await;
    delete(a, &mut model, &["d00010"], false).await;
    upsert(
        a,
        &mut model,
        &[("d99999".to_owned(), attrs(99_999))],
        false,
    )
    .await;
    let (e, m) = past.unwrap();
    (model, e, m)
}

#[tokio::test]
async fn counts_and_sums_equal_brute_force() {
    let w = world(&[1]);
    let a = &w.apis[0];
    let (model, past_epoch, past_model) = seeded(a).await;
    for filter in [None, Some(50)] {
        let got = query(a, &aggregates(filter)).await;
        check(
            &got,
            expected(&model, filter),
            &format!("filter {filter:?}"),
        );
        assert!(got["meta"]["unfolded_hits"].as_u64().unwrap() > 0, "{got}");
        let mut past = aggregates(filter);
        past["as_of"] = json!(past_epoch);
        let got = query(a, &past).await;
        check(
            &got,
            expected(&past_model, filter),
            &format!("as_of, filter {filter:?}"),
        );
    }
}

#[tokio::test]
async fn groups_equal_brute_force() {
    let w = world(&[1]);
    let a = &w.apis[0];
    let (model, _, _) = seeded(a).await;
    // One attribute: five colors, then the absent group last.
    let q = json!({"aggregate_by": {"n": ["Count", "id"], "price": ["Sum", "price"]},
        "group_by": ["color"], "top_k": 100});
    let got = query(a, &q).await;
    let mut want: BTreeMap<String, (u64, i64)> = BTreeMap::new();
    let mut absent = (0u64, 0i64);
    for r in model.values() {
        let slot = match r.get("color") {
            Some(c) => want.entry(c.as_str().unwrap().to_owned()).or_default(),
            None => &mut absent,
        };
        slot.0 += 1;
        slot.1 += r["price"].as_i64().unwrap();
    }
    let mut expect: Vec<Value> = want
        .iter()
        .map(|(c, (n, p))| json!({"color": c, "n": n, "price": p}))
        .collect();
    expect.push(json!({"color": null, "n": absent.0, "price": absent.1}));
    assert_eq!(got["aggregation_groups"], json!(expect));

    // Two attributes: color, then the bool, lexicographically.
    let q = json!({"aggregate_by": {"n": ["Count", "id"]}, "group_by": ["color", "even"],
        "top_k": 100});
    let got = query(a, &q).await;
    let mut want: BTreeMap<(u8, String, bool), u64> = BTreeMap::new();
    for r in model.values() {
        let (rank, c) = match r.get("color") {
            Some(c) => (0, c.as_str().unwrap().to_owned()),
            None => (1, String::new()),
        };
        *want
            .entry((rank, c, r["even"].as_bool().unwrap()))
            .or_default() += 1;
    }
    let expect: Vec<Value> = want
        .iter()
        .map(|((rank, c, e), n)| {
            let c = if *rank == 0 { json!(c) } else { Value::Null };
            json!({"color": c, "even": e, "n": n})
        })
        .collect();
    assert_eq!(got["aggregation_groups"], json!(expect));
}

#[tokio::test]
async fn the_smallest_keys_are_exact_across_segments() {
    let w = world(&[1]);
    let a = &w.apis[0];
    let mut model = Model::new();
    let one = |id: &str, k: &str| {
        let mut m = Map::new();
        m.insert("k".into(), json!(k));
        (id.to_owned(), m)
    };
    // Segment 1: c is its own 3rd smallest and one of the global 3 smallest.
    let segments = [
        vec![
            one("s1a", "a"),
            one("s1b", "b"),
            one("s1c", "c"),
            one("s1x", "x"),
            one("s1y", "y"),
        ],
        vec![one("s2c", "c"), one("s2d", "d"), one("s2c2", "c")],
        vec![one("s3b", "b"), one("s3z", "z"), one("s3a", "a")],
    ];
    for rows in &segments {
        upsert(a, &mut model, rows, true).await;
        fold(a).await;
    }
    let q = json!({"aggregate_by": {"n": ["Count", "id"]}, "group_by": ["k"], "top_k": 3});
    let got = query(a, &q).await;
    assert_eq!(
        got["aggregation_groups"],
        json!([{"k": "a", "n": 2}, {"k": "b", "n": 2}, {"k": "c", "n": 3}])
    );
}

#[tokio::test]
async fn numbers_and_arrays_group_by_value() {
    let w = world(&[1]);
    let a = &w.apis[0];
    let mut model = Model::new();
    let one = |id: &str, k: Value| {
        let mut m = Map::new();
        if !k.is_null() {
            m.insert("k".into(), k);
        }
        (id.to_owned(), m)
    };
    upsert(
        a,
        &mut model,
        &[
            one("a", json!(1)),
            one("b", json!(1.0)),
            one("c", json!(2.5)),
            one("d", Value::Null),
            one("e", json!("x")),
        ],
        true,
    )
    .await;
    fold(a).await;
    let q = json!({"aggregate_by": {"n": ["Count", "id"]}, "group_by": ["k"]});
    let got = query(a, &q).await;
    assert_eq!(
        got["aggregation_groups"],
        json!([{"k": 1, "n": 2}, {"k": 2.5, "n": 1}, {"k": "x", "n": 1}, {"k": null, "n": 1}])
    );
    upsert(
        a,
        &mut model,
        &[
            one("f", json!([1])),
            one("g", json!([1.0])),
            one("h", json!([1, 2])),
        ],
        true,
    )
    .await;
    fold(a).await;
    let q = json!({"aggregate_by": {"n": ["Count", "id"]}, "group_by": ["k"],
        "filters": ["k", "ContainsAny", [1, 2]]});
    let got = query(a, &q).await;
    let groups = got["aggregation_groups"].as_array().unwrap();
    let arrays: Vec<&Value> = groups.iter().filter(|g| g["k"].is_array()).collect();
    assert_eq!(
        arrays,
        [&json!({"k": [1], "n": 2}), &json!({"k": [1, 2], "n": 1})],
        "{got}"
    );
}

fn count() -> Value {
    json!({"aggregate_by": {"n": ["Count", "id"]}})
}

#[tokio::test]
async fn an_unfiltered_count_is_heads_arithmetic() {
    let w = world(&[1, 2]);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    let mut model = Model::new();
    for k in 0..4 {
        upsert(a, &mut model, &rows(k * 500..(k + 1) * 500), true).await;
        fold(a).await;
    }
    let past = fold(a).await;
    let past_model = model.clone();
    delete(a, &mut model, &["d00001", "d00002", "d01999"], true).await;
    fold(a).await;
    // B holds nothing unfolded: one read, no segment opened.
    let got = query(b, &count()).await;
    assert_eq!(got["aggregations"]["n"], model.len() as u64, "{got}");
    assert_eq!(got["meta"]["cost"]["blob_reads"], 1, "{got}");
    // The full path, forced by a filter admitting everything, agrees and costs more.
    let mut all = count();
    all["filters"] = json!(["price", "Gt", 0]);
    let full = query(b, &all).await;
    assert_eq!(full["aggregations"]["n"], got["aggregations"]["n"]);
    assert!(full["meta"]["cost"]["blob_reads"].as_u64().unwrap() > 1);
    // As of a past epoch.
    let mut then = count();
    then["as_of"] = json!(past);
    assert_eq!(
        query(b, &then).await["aggregations"]["n"],
        past_model.len() as u64
    );
    // Unfolded on B: the full path, still exact -- a write, then a delete of a folded id.
    upsert(b, &mut model, &rows(5000..5001), false).await;
    let got = query(b, &count()).await;
    assert_eq!(got["aggregations"]["n"], model.len() as u64, "{got}");
    assert!(got["meta"]["cost"]["blob_reads"].as_u64().unwrap() > 1);
    delete(b, &mut model, &["d00100"], false).await;
    assert_eq!(
        query(b, &count()).await["aggregations"]["n"],
        model.len() as u64
    );
    // A missing index is 404, on either path.
    for q in [count(), all] {
        let (s, body, _) = send(b, "POST", "/v1/indexes/nothing/query", &q, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    }
}

#[tokio::test]
async fn a_filtered_aggregation_is_three_rounds_deep() {
    let w = world(&[1]);
    let a = &w.apis[0];
    let mut model = Model::new();
    for k in 0..3 {
        upsert(a, &mut model, &rows(k * 300..(k + 1) * 300), true).await;
        fold(a).await;
    }
    w.depth.reset();
    let q = json!({"aggregate_by": {"n": ["Count", "id"], "s": ["Sum", "price"]},
        "group_by": ["color"], "filters": ["price", "Gt", 50]});
    query(a, &q).await;
    assert!(w.depth.depth() <= 3, "depth {}", w.depth.depth());
}

#[tokio::test(start_paused = true)]
async fn every_level_applies() {
    let w = world(&[1, 2, 3]);
    let (a, b, c) = (&w.apis[0], &w.apis[1], &w.apis[2]);
    let mut model = Model::new();
    upsert(a, &mut model, &rows(0..10), true).await;
    fold(a).await;
    let token = upsert(a, &mut model, &rows(10..11), true).await;
    let mut filtered = count();
    filtered["filters"] = json!(["price", "Gt", 0]);
    for q in [count(), filtered.clone()] {
        let mut strong = q.clone();
        strong["consistency"] = json!("strong");
        let (s, body, _) = send(b, "POST", "/v1/indexes/docs/query", &strong, None).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        let mut session = q.clone();
        session["consistency"] = json!("session");
        let (s, body, _) = send(b, "POST", "/v1/indexes/docs/query", &session, Some(&token)).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    }
    // A reader per shape: a HEAD one shape cached would make the other's first read a hit.
    for (reader, q) in [(b, count()), (c, filtered)] {
        let mut bounded = q.clone();
        bounded["consistency"] = json!("bounded");
        bounded["max_staleness_ms"] = json!(60_000);
        let miss = query(reader, &bounded).await;
        assert_eq!(miss["meta"]["staleness_ms"], 0, "{miss}");
        tokio::time::advance(Duration::from_millis(10)).await;
        let hit = query(reader, &bounded).await;
        assert_eq!(hit["meta"]["staleness_ms"], 10, "{hit}");
        assert_eq!(hit["aggregations"]["n"], 10, "{hit}");
        assert_eq!(
            hit["meta"]["cost"]["blob_reads"].as_u64().unwrap() + 1,
            miss["meta"]["cost"]["blob_reads"].as_u64().unwrap(),
            "a hit reads no HEAD: {hit}"
        );
    }
    let mut bounded = count();
    bounded["consistency"] = json!("bounded");
    bounded["max_staleness_ms"] = json!(60_000);
    let hit = query(b, &bounded).await;
    assert_eq!(
        hit["meta"]["cost"]["blob_reads"], 0,
        "a fast-path hit reads nothing: {hit}"
    );
}

#[tokio::test]
async fn what_an_aggregation_cannot_mean_is_refused() {
    let w = world(&[1]);
    let a = &w.apis[0];
    upsert(a, &mut Model::new(), &rows(0..3), true).await;
    let base = count();
    let with = |k: &str, v: Value| {
        let mut q = base.clone();
        q[k] = v;
        q
    };
    let seventeen: Map<String, Value> = (0..17)
        .map(|i| (format!("l{i}"), json!(["Count", "id"])))
        .collect();
    let nine: Vec<String> = (0..9).map(|i| format!("a{i}")).collect();
    for (q, why) in [
        (with("vector", json!([1.0, 0.5])), "aggregate_by"),
        (with("text", json!("x")), "aggregate_by"),
        (with("rank_by", json!(["id", "asc"])), "aggregate_by"),
        (with("offset", json!(1)), "aggregate_by"),
        (with("include_attributes", json!(true)), "aggregate_by"),
        (with("exclude_attributes", json!(["x"])), "aggregate_by"),
        (
            json!({"vector": [1.0, 0.5], "group_by": ["color"]}),
            "group_by",
        ),
        (json!({"aggregate_by": {"n": ["Avg", "price"]}}), "Avg"),
        (json!({"aggregate_by": {"n": ["Sum", "id"]}}), "Sum"),
        (json!({"aggregate_by": {"n": ["Count"]}}), "aggregate"),
        (json!({"aggregate_by": {"n": "Count"}}), "aggregate"),
        (json!({"aggregate_by": {}}), "aggregate_by"),
        (json!({"aggregate_by": seventeen}), "aggregate_by"),
        (with("group_by", json!([])), "group_by"),
        (with("group_by", json!(nine)), "group_by"),
        (with("group_by", json!(["n"])), "label"),
        (json!({"queries": [count()]}), "multi-query"),
    ] {
        let (s, body, _) = send(a, "POST", "/v1/indexes/docs/query", &q, None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{q} was accepted: {body}");
        let msg = body["error"]["message"].as_str().unwrap();
        assert!(msg.contains(why), "{q}: {msg:?} does not name {why:?}");
    }
}

#[tokio::test]
async fn an_unfolded_delete_alone_takes_the_full_path() {
    // Code review, M2: a reader whose only unfolded operation is a delete of a folded id.
    let w = world(&[1, 2]);
    let (a, b) = (&w.apis[0], &w.apis[1]);
    let mut model = Model::new();
    upsert(a, &mut model, &rows(0..50), true).await;
    fold(a).await;
    delete(b, &mut model, &["d00007"], false).await;
    let got = query(b, &count()).await;
    assert_eq!(got["aggregations"]["n"], model.len() as u64, "{got}");
    assert_eq!(got["aggregations"]["n"], 49);
}

#[tokio::test]
async fn an_aggregations_top_k_is_bounded_as_an_orders_is() {
    let w = world(&[1]);
    let a = &w.apis[0];
    upsert(a, &mut Model::new(), &rows(0..3), true).await;
    let mut q = count();
    q["group_by"] = json!(["color"]);
    q["top_k"] = json!(10_001);
    let (s, body, _) = send(a, "POST", "/v1/indexes/docs/query", &q, None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("top_k"));
    q["top_k"] = json!(10_000);
    query(a, &q).await;
}
