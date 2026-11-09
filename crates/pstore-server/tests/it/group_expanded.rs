//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Expanded grouping (M211): `Min`, `Max`, `Avg` and numeric buckets, against a reference over
//! the rows written.

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

const TENANT: u128 = 43;

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

/// Row `i`'s price: an int, a float, a string or absent in a fixed pattern, some negative.
fn price(i: usize) -> Option<Value> {
    let n = i64::try_from(i).unwrap();
    match i % 5 {
        0 => None,
        1 => Some(json!(n * 3 - 20)),
        2 => Some(json!(n as f64 * 1.5 - 7.25)),
        3 => Some(json!("cheap")),
        _ => Some(json!(n * 7 % 23)),
    }
}

fn attrs(i: usize) -> Map<String, Value> {
    let mut a = Map::new();
    a.insert("n".into(), json!(i));
    let color = ["red", "blue", "green", "grey"][i % 4];
    a.insert("color".into(), json!(color));
    let tag = ["x", "y"][i % 2];
    a.insert("tags".into(), json!([tag, "z"]));
    if let Some(p) = price(i) {
        a.insert("price".into(), p);
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

/// 40 rows: 0..30 folded in two segments, 30..40 unfolded.
async fn corpus() -> (A, Store) {
    let store = Accounted::new(DepthCounting::new(MemoryStore::new()));
    let api = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    write(&api, 0..15, true).await;
    fold(&api).await;
    write(&api, 15..30, true).await;
    fold(&api).await;
    write(&api, 30..40, false).await;
    (api, store)
}

fn stats() -> Value {
    json!({"lo": ["Min", "price"], "hi": ["Max", "price"], "avg": ["Avg", "price"],
           "sum": ["Sum", "price"], "c": ["Count", "price"]})
}

/// The reference's totals over the numeric prices of `rows`.
fn reference(rows: &[usize]) -> Map<String, Value> {
    let nums: Vec<f64> = rows
        .iter()
        .filter_map(|i| price(*i))
        .filter_map(|p| p.as_f64())
        .collect();
    let counted = rows.iter().filter(|i| price(**i).is_some()).count();
    let mut m = Map::new();
    let as_json = |f: f64| {
        if f.fract() == 0.0 {
            json!(f as i64)
        } else {
            json!(f)
        }
    };
    let lo = nums.iter().copied().reduce(f64::min);
    let hi = nums.iter().copied().reduce(f64::max);
    m.insert("lo".into(), lo.map_or(Value::Null, as_json));
    m.insert("hi".into(), hi.map_or(Value::Null, as_json));
    let sum: f64 = nums.iter().sum();
    m.insert(
        "avg".into(),
        if nums.is_empty() {
            Value::Null
        } else {
            json!(sum / nums.len() as f64)
        },
    );
    m.insert("sum".into(), as_json(sum));
    m.insert("c".into(), json!(counted));
    m
}

/// Equal, with `avg` and `sum` within 1e-9 relative.
fn same(got: &Value, want: &Map<String, Value>, what: &str) {
    for (k, w) in want {
        let g = &got[k];
        match (g.as_f64(), w.as_f64()) {
            (Some(g), Some(w)) if k == "avg" || k == "sum" => {
                assert!(
                    (g - w).abs() <= w.abs() * 1e-9 + 1e-12,
                    "{what} {k}: {g} != {w}"
                );
            }
            _ => assert_eq!(g, w, "{what} {k}: {got}"),
        }
    }
}

#[tokio::test]
async fn min_max_and_avg_equal_a_reference_through_folded_and_unfolded_rows() {
    let (api, _) = corpus().await;
    for filter in [false, true] {
        let keep = |i: usize| !filter || i >= 12;
        let mut q = json!({"aggregate_by": stats()});
        if filter {
            q["filters"] = json!(["n", "Gte", 12]);
        }
        let all: Vec<usize> = (0..40).filter(|i| keep(*i)).collect();
        let got = query(&api, &q).await;
        same(&got["aggregations"], &reference(&all), "ungrouped");
        q["group_by"] = json!(["color"]);
        let got = query(&api, &q).await;
        let groups = got["aggregation_groups"].as_array().unwrap();
        let colors = ["blue", "green", "grey", "red"];
        assert_eq!(groups.len(), 4, "{got}");
        for (g, color) in groups.iter().zip(colors) {
            let rows: Vec<usize> = all
                .iter()
                .copied()
                .filter(|i| attrs(*i)["color"] == color)
                .collect();
            assert_eq!(g["color"], color);
            same(g, &reference(&rows), color);
        }
    }
    // A group holding no number reports null for all three.
    let q = json!({"aggregate_by": stats(), "filters": ["price", "Eq", "cheap"]});
    let got = query(&api, &q).await;
    for k in ["lo", "hi", "avg"] {
        assert_eq!(got["aggregations"][k], Value::Null, "{got}");
    }
    assert_eq!(got["aggregations"]["c"], 8, "{got}");
}

fn bucket_of(p: &Value, width: f64) -> Value {
    p.as_f64().map_or(Value::Null, |v| {
        let b = (v / width).floor() * width;
        if b.fract() == 0.0 {
            json!(b as i64)
        } else {
            json!(b)
        }
    })
}

/// The reference's groups by (bucket of price[, color][, tag]), counted, ascending, null last.
fn bucketed(width: f64, by_color: bool, by_tag: bool, top_k: usize) -> Value {
    let mut groups: BTreeMap<(u8, i64, String, String), (Value, u64)> = BTreeMap::new();
    for i in 0..40 {
        let a = attrs(i);
        let b = a.get("price").map_or(Value::Null, |p| bucket_of(p, width));
        let color = if by_color {
            a["color"].as_str().unwrap().to_owned()
        } else {
            String::new()
        };
        let tags: Vec<String> = if by_tag {
            a["tags"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap().to_owned())
                .collect()
        } else {
            vec![String::new()]
        };
        // Ordered by the bound, scaled to an integer: every bound is a multiple of 0.5.
        let rank = b.as_f64().map_or((1, 0), |f| (0, (f * 2.0) as i64));
        for t in tags {
            let g = groups
                .entry((rank.0, rank.1, color.clone(), t))
                .or_insert((b.clone(), 0));
            g.1 += 1;
        }
    }
    Value::Array(
        groups
            .into_iter()
            .take(top_k)
            .map(|((_, _, color, tag), (b, c))| {
                let mut row = json!({"band": b, "c": c});
                if by_color {
                    row["color"] = json!(color);
                }
                if by_tag {
                    row["tag"] = json!(tag);
                }
                row
            })
            .collect(),
    )
}

fn banded(width: Value, by_color: bool, by_tag: bool, top_k: usize) -> Value {
    let mut group_by = vec![json!({"band": ["Bucket", "price", width]})];
    if by_color {
        group_by.push(json!("color"));
    }
    if by_tag {
        group_by.push(json!({"tag": ["ForEachUnique", "tags"]}));
    }
    json!({"aggregate_by": {"c": ["Count"]}, "group_by": group_by, "top_k": top_k})
}

#[tokio::test]
async fn buckets_are_floored_lower_bounds_with_non_numbers_last() {
    let (api, _) = corpus().await;
    let got = query(&api, &banded(json!(10), false, false, 100)).await;
    let want = bucketed(10.0, false, false, 100);
    assert_eq!(got["aggregation_groups"], want, "{got}");
    // A negative price falls below zero: -20 + 3 = -17 in -20, and -1 would be in -10.
    assert!(
        want.as_array().unwrap().iter().any(|g| g["band"] == -20),
        "{want}"
    );
    let got = query(&api, &banded(json!(2.5), false, false, 100)).await;
    assert_eq!(
        got["aggregation_groups"],
        bucketed(2.5, false, false, 100),
        "{got}"
    );
}

#[tokio::test]
async fn a_bucket_combines_with_plain_and_exploded_keys_and_is_bounded() {
    let (api, _) = corpus().await;
    let got = query(&api, &banded(json!(10), true, true, 1000)).await;
    assert_eq!(
        got["aggregation_groups"],
        bucketed(10.0, true, true, 1000),
        "{got}"
    );
    for top_k in [1, 2] {
        let got = query(&api, &banded(json!(10), false, false, top_k)).await;
        assert_eq!(
            got["aggregation_groups"],
            bucketed(10.0, false, false, top_k),
            "{got}"
        );
    }
}

#[tokio::test]
async fn a_multi_query_and_facets_take_expanded_grouping() {
    let (api, _) = corpus().await;
    let q = json!({"aggregate_by": stats(), "group_by": ["color"]});
    let alone = query(&api, &q).await;
    let ranked = json!({"vector": [1.0, 0.5], "top_k": 2});
    let multi = query(&api, &json!({"queries": [ranked.clone(), q]})).await;
    assert_eq!(
        multi["aggregation_groups"][1], alone["aggregation_groups"],
        "{multi}"
    );
    let banded_alone = query(&api, &banded(json!(10), false, false, 100)).await;
    let mut faceted = ranked;
    faceted["facets"] = json!({"aggregate_by": {"c": ["Count"]},
        "group_by": [{"band": ["Bucket", "price", 10]}], "top_k": 100});
    let got = query(&api, &faceted).await;
    assert_eq!(got["facets"], banded_alone["aggregation_groups"], "{got}");
}

#[tokio::test]
async fn what_expanded_grouping_cannot_mean_is_refused_before_any_blob_request() {
    let (api, store) = corpus().await;
    let spent =
        || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(TENANT), c));
    let count = json!({"c": ["Count"]});
    for (aggregate_by, group_by, why) in [
        (json!({"m": ["Min", "id"]}), None, "Min of id"),
        (json!({"m": ["Max", "id"]}), None, "Max of id"),
        (json!({"m": ["Avg", "id"]}), None, "Avg of id"),
        (json!({"m": ["Avg"]}), None, "aggregate m"),
        (json!({"m": ["Median", "price"]}), None, "Median"),
        (
            count.clone(),
            Some(json!([{"b": ["Bucket", "price", 0]}])),
            "Bucket",
        ),
        (
            count.clone(),
            Some(json!([{"b": ["Bucket", "price", -1]}])),
            "Bucket",
        ),
        (
            count.clone(),
            Some(json!([{"b": ["Bucket", "price", "10"]}])),
            "Bucket",
        ),
        (
            count.clone(),
            Some(json!([{"b": ["Bucket", "price"]}])),
            "Bucket",
        ),
        (
            count.clone(),
            Some(json!([{"b": ["Bucket", 7, 10]}])),
            "Bucket",
        ),
        (
            count.clone(),
            Some(json!([{"b": ["Bucket", "price", 10], "c": ["Bucket", "n", 1]}])),
            "Bucket",
        ),
        (
            count.clone(),
            Some(json!(["b", {"b": ["Bucket", "price", 10]}])),
            "twice",
        ),
        (
            count.clone(),
            Some(json!([{"b": ["Bucket", "price", 10]}, {"b": ["ForEachUnique", "tags"]}])),
            "twice",
        ),
    ] {
        let mut q = json!({"aggregate_by": aggregate_by});
        if let Some(g) = group_by {
            q["group_by"] = g;
        }
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
