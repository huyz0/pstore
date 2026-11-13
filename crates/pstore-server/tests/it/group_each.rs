//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Named `ForEachUnique` groups and unary `Count` (M210), against a reference that counts each
//! row once per unique element of its array.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_server::Api;
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type Store = Accounted<DepthCounting<MemoryStore>>;
type A = Arc<Api<DepthCounting<MemoryStore>>>;

const TENANT: u128 = 41;
const TAGS: [&str; 5] = ["a", "b", "c", "d", "e"];

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT.to_string())
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(r).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn query(api: &A, q: &Value) -> Value {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", q).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b
}

/// Row `i`'s attributes: `tags` of 0 to 3 elements, some repeated, some a scalar, some none.
fn attrs(i: usize) -> Map<String, Value> {
    let mut a = Map::new();
    a.insert("n".into(), json!(i + 1));
    a.insert(
        "color".into(),
        json!(if i.is_multiple_of(3) { "red" } else { "blue" }),
    );
    let tags = match i % 6 {
        0 => None,
        1 => Some(json!(TAGS[i % 5])),
        2 => Some(json!([])),
        3 => Some(json!([TAGS[i % 5], TAGS[(i + 1) % 5]])),
        4 => Some(json!([TAGS[i % 5], TAGS[i % 5], TAGS[(i + 2) % 5]])),
        _ => Some(json!([TAGS[(i + 3) % 5]])),
    };
    if let Some(t) = tags {
        a.insert("tags".into(), t);
    }
    a
}

async fn write(api: &A, range: std::ops::Range<usize>, durable: bool) {
    let documents: Vec<Value> = range
        .map(|i| json!({"id": format!("d{i:03}"), "vector": [1.0, 0.5], "attributes": attrs(i)}))
        .collect();
    let body =
        json!({"durability": if durable { "durable" } else { "batched" }, "documents": documents});
    let (s, b) = send(api, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// 30 rows: 0..20 folded in two segments, 20..30 unfolded.
async fn corpus() -> (A, Store, DepthCounting<MemoryStore>) {
    let depth = DepthCounting::new(MemoryStore::new());
    let store = Accounted::new(depth.clone());
    let api = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    write(&api, 0..10, true).await;
    fold(&api).await;
    write(&api, 10..20, true).await;
    fold(&api).await;
    write(&api, 20..30, false).await;
    (api, store, depth)
}

/// A row's unique elements, as the reference keys them: absent and `[]` are `null`.
fn elements(a: &Map<String, Value>) -> Vec<Value> {
    match a.get("tags") {
        Some(Value::Array(v)) if !v.is_empty() => {
            let mut e: Vec<String> = v.iter().map(|t| t.as_str().unwrap().to_owned()).collect();
            e.sort();
            e.dedup();
            e.into_iter().map(Value::String).collect()
        }
        Some(Value::String(s)) => vec![json!(s)],
        _ => vec![Value::Null],
    }
}

/// The groups the reference computes: per (element[, color]), the count and the sum of `n`.
fn reference(keep: impl Fn(usize) -> bool, by_color: bool, top_k: usize) -> Value {
    let mut groups: BTreeMap<(u8, String, String), (u64, u64, Value)> = BTreeMap::new();
    for i in (0..30).filter(|i| keep(*i)) {
        let a = attrs(i);
        for e in elements(&a) {
            // Strings first, absent last, as keys order.
            let rank = (u8::from(e.is_null()), e.as_str().unwrap_or("").to_owned());
            let color = if by_color {
                a["color"].as_str().unwrap().to_owned()
            } else {
                String::new()
            };
            let g = groups
                .entry((rank.0, rank.1, color))
                .or_insert((0, 0, e.clone()));
            g.0 += 1;
            g.1 += a["n"].as_u64().unwrap();
        }
    }
    let rows: Vec<Value> = groups
        .into_iter()
        .take(top_k)
        .map(|((_, _, color), (c, s, tag))| {
            let mut row = json!({"tag": tag, "c": c, "s": s});
            if by_color {
                row["color"] = json!(color);
            }
            row
        })
        .collect();
    Value::Array(rows)
}

fn exploded(filter: bool, by_color: bool, top_k: usize) -> Value {
    let mut group_by = vec![json!({"tag": ["ForEachUnique", "tags"]})];
    if by_color {
        group_by.push(json!("color"));
    }
    let mut q = json!({
        "aggregate_by": {"c": ["Count"], "s": ["Sum", "n"]},
        "group_by": group_by,
        "top_k": top_k,
    });
    if filter {
        q["filters"] = json!(["n", "Gt", 12]);
    }
    q
}

#[tokio::test]
async fn each_unique_element_is_one_group_through_folded_and_unfolded_rows() {
    let (api, _, _) = corpus().await;
    for filter in [false, true] {
        let got = query(&api, &exploded(filter, false, 100)).await;
        let want = reference(|i| !filter || i + 1 > 12, false, 100);
        assert_eq!(got["aggregation_groups"], want, "filter {filter}: {got}");
        // Unfolded rows 20..30, each counted once however many of its groups were kept.
        let unfolded = (20..30).filter(|i| !filter || i + 1 > 12).count();
        assert_eq!(got["meta"]["unfolded_hits"], unfolded, "{got}");
    }
}

#[tokio::test]
async fn an_element_twice_in_one_row_counts_the_row_once() {
    let api: A = Api::unsecured(
        Accounted::new(DepthCounting::new(MemoryStore::new())),
        LaneId(1),
    )
    .unwrap();
    let body = json!({"documents": [
        {"id": "x", "vector": [1.0, 0.5], "attributes": {"tags": ["a", "a", "b"], "n": 7}},
        {"id": "y", "vector": [1.0, 0.5], "attributes": {"nums": [1, 1.0], "n": 2}},
    ]});
    let (s, b) = send(&api, "PUT", "/v1/indexes/docs/documents", &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let q = json!({"aggregate_by": {"c": ["Count"], "s": ["Sum", "n"]},
        "group_by": [{"tag": ["ForEachUnique", "tags"]}], "filters": ["id", "Eq", "x"]});
    assert_eq!(
        query(&api, &q).await["aggregation_groups"],
        json!([{"tag": "a", "c": 1, "s": 7}, {"tag": "b", "c": 1, "s": 7}])
    );
    let q = json!({"aggregate_by": {"c": ["Count"]},
        "group_by": [{"v": ["ForEachUnique", "nums"]}], "filters": ["id", "Eq", "y"]});
    assert_eq!(
        query(&api, &q).await["aggregation_groups"],
        json!([{"v": 1, "c": 1}])
    );
}

#[tokio::test]
async fn an_exploded_key_combines_with_a_plain_one_in_key_order() {
    let (api, _, _) = corpus().await;
    let got = query(&api, &exploded(false, true, 100)).await;
    assert_eq!(
        got["aggregation_groups"],
        reference(|_| true, true, 100),
        "{got}"
    );
}

#[tokio::test]
async fn the_smallest_elements_are_exact_across_segments() {
    let (api, _, _) = corpus().await;
    for top_k in [1, 2] {
        let got = query(&api, &exploded(false, false, top_k)).await;
        assert_eq!(
            got["aggregation_groups"],
            reference(|_| true, false, top_k),
            "{got}"
        );
    }
}

#[tokio::test]
async fn a_multi_query_and_facets_take_named_groups() {
    let (api, _, _) = corpus().await;
    let alone = query(&api, &exploded(true, false, 100)).await;
    let ranked = json!({"vector": [1.0, 0.5], "top_k": 2});
    let multi = query(
        &api,
        &json!({"queries": [ranked, exploded(true, false, 100)]}),
    )
    .await;
    assert_eq!(
        multi["aggregation_groups"][1], alone["aggregation_groups"],
        "{multi}"
    );
    let faceted = json!({"vector": [1.0, 0.5], "top_k": 2, "filters": ["n", "Gt", 12],
        "facets": {"aggregate_by": {"c": ["Count"], "s": ["Sum", "n"]},
                   "group_by": [{"tag": ["ForEachUnique", "tags"]}], "top_k": 100}});
    let got = query(&api, &faceted).await;
    assert_eq!(got["facets"], alone["aggregation_groups"], "{got}");
}

#[tokio::test]
async fn unary_count_counts_rows_and_takes_the_fast_path() {
    let (api, store, _) = corpus().await;
    // The batched rows made durable and folded, so a second server holds nothing unfolded.
    write(&api, 20..30, true).await;
    fold(&api).await;
    let api = Api::unsecured(store.clone(), LaneId(2)).unwrap();
    for filter in [None, Some(json!(["n", "Gt", 12]))] {
        let mut unary = json!({"aggregate_by": {"n": ["Count"]}});
        let mut named = json!({"aggregate_by": {"n": ["Count", "id"]}});
        if let Some(f) = &filter {
            unary["filters"] = f.clone();
            named["filters"] = f.clone();
        }
        let got = query(&api, &unary).await;
        assert_eq!(
            got["aggregations"],
            query(&api, &named).await["aggregations"]
        );
        let want = if filter.is_some() { 18 } else { 30 };
        assert_eq!(got["aggregations"]["n"], want, "{got}");
    }
    // Unfiltered, nothing unfolded: HEAD's arithmetic, one read.
    let before = store.count(TenantId(TENANT), OpClass::Read);
    let got = query(&api, &json!({"aggregate_by": {"n": ["Count"]}})).await;
    assert_eq!(got["meta"]["cost"]["blob_reads"], 1, "{got}");
    assert!(store.count(TenantId(TENANT), OpClass::Read) - before <= 1);
}

#[tokio::test]
async fn what_a_named_group_cannot_mean_is_refused_before_any_blob_request() {
    let (api, store, _) = corpus().await;
    let spent =
        || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(TENANT), c));
    let each = json!({"tag": ["ForEachUnique", "tags"]});
    for (group_by, why) in [
        (
            json!([each, {"t2": ["ForEachUnique", "color"]}]),
            "one ForEachUnique",
        ),
        (json!([{"t": ["Avg", "tags"]}]), "ForEachUnique"),
        (json!([{"t": "tags"}]), "ForEachUnique"),
        (json!([{"t": ["ForEachUnique"]}]), "ForEachUnique"),
        (
            json!([{"t": ["ForEachUnique", "tags"], "u": ["ForEachUnique", "tags"]}]),
            "ForEachUnique",
        ),
        (json!([{"c": ["ForEachUnique", "tags"]}]), "label"),
        (
            json!([{"color": ["ForEachUnique", "tags"]}, "color"]),
            "twice",
        ),
        (json!(["tags", each]), "twice"),
    ] {
        let q = json!({"aggregate_by": {"c": ["Count"]}, "group_by": group_by});
        for body in [q.clone(), json!({"queries": [q]})] {
            let before = spent();
            let (s, b) = send(&api, "POST", "/v1/indexes/docs/query", &body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
            let msg = b["error"]["message"].as_str().unwrap();
            assert!(msg.contains(why), "{body}: {msg:?} does not name {why:?}");
            assert_eq!(before, spent(), "{body}: the refusal made blob requests");
        }
    }
}
