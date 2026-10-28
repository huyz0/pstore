//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A hard per-group limit on a relevance ranking through the API — M146. Every limited
//! answer is checked against the same query run plain at `top_k = window`, cut by the
//! test's own walk.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_server::Api;
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceExt;

async fn send<S: BlobStore + 'static>(
    api: &Arc<Api<S>>,
    method: &str,
    uri: &str,
    body: Option<&Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "22")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn query<S: BlobStore + 'static>(api: &Arc<Api<S>>, body: &Value) -> Value {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{body} -> {b}");
    b
}

/// Forty documents. `g` is an int, an equal float, absent, or a string; `pop` a number.
fn doc(i: usize) -> Value {
    let x = i as f32;
    let mut attrs = json!({"pop": (i * 37 % 101) as i64});
    match i % 4 {
        0 => attrs["g"] = json!(i % 3),
        1 => attrs["g"] = json!((i % 3) as f64),
        2 => {}
        _ => attrs["g"] = json!(format!("s{}", i % 5)),
    }
    json!({"id": format!("d{i:02}"), "vector": [x.cos(), x.sin(), 1.0, 0.5],
           "text": format!("common word{}", i % 5), "attributes": attrs})
}

async fn corpus<S: BlobStore + 'static>(store: Accounted<S>) -> Arc<Api<S>> {
    let api = Api::unsecured(store, LaneId(1)).unwrap();
    let docs: Vec<Value> = (0..40).map(doc).collect();
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": docs})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(&api, "POST", "/v1/admin/fold", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    api
}

/// A group's key as the test sees it: numbers by value, absent as `-`.
fn key(r: &Value) -> String {
    match &r["attributes"]["g"] {
        Value::Null => "-".to_owned(),
        Value::Number(n) => format!("n{}", n.as_f64().unwrap()),
        other => format!("{other}"),
    }
}

fn ids(b: &Value) -> Vec<String> {
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The plain answer at the window, walked: at most `m` a group, `top_k` in all.
fn cut(plain: &Value, m: usize, top_k: usize) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut kept = Vec::new();
    for r in plain["results"].as_array().unwrap() {
        if kept.len() == top_k {
            break;
        }
        let c = seen.entry(key(r)).or_insert(0);
        if *c < m {
            *c += 1;
            kept.push(r["id"].as_str().unwrap().to_owned());
        }
    }
    kept
}

fn most_of_one_group(b: &Value) -> usize {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for r in b["results"].as_array().unwrap() {
        *seen.entry(key(r)).or_insert(0) += 1;
    }
    seen.into_values().max().unwrap_or(0)
}

#[tokio::test]
async fn the_limit_cuts_the_windows_ranking_per_group() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    let v = json!([1.0, 0.2, 1.0, 0.5]);
    for (m, top_k, window) in [(1, 5, None), (2, 12, Some(30)), (3, 20, Some(40))] {
        let mut per = json!({"attributes": ["g"], "limit": m});
        if let Some(w) = window {
            per["window"] = json!(w);
        }
        let w = window.unwrap_or(100);
        let plain = query(
            &api,
            &json!({"vector": v, "top_k": w, "include_attributes": ["g"]}),
        )
        .await;
        let first = json!({"results": plain["results"].as_array().unwrap()[..top_k]});
        assert!(
            most_of_one_group(&first) > m,
            "the fixture proves nothing at m {m}"
        );
        let got = query(
            &api,
            &json!({"vector": v, "top_k": top_k, "limit_per": per, "include_attributes": ["g"]}),
        )
        .await;
        assert_eq!(ids(&got), cut(&plain, m, top_k), "m {m}, top_k {top_k}");
        assert!(most_of_one_group(&got) <= m, "{got}");
    }
    // Without `include_attributes`, the groups are read all the same.
    let bare = query(
        &api,
        &json!({"vector": v, "top_k": 5, "limit_per": {"attributes": ["g"], "limit": 1}}),
    )
    .await;
    let plain = query(
        &api,
        &json!({"vector": v, "top_k": 100, "include_attributes": ["g"]}),
    )
    .await;
    assert_eq!(ids(&bare), cut(&plain, 1, 5));
}

#[tokio::test]
async fn with_a_boost_the_cut_is_of_the_boosted_order() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    let v = json!([1.0, 0.2, 1.0, 0.5]);
    let terms = json!([{"attr": "pop", "fn": "linear", "weight": 0.01}]);
    let plain = query(
        &api,
        &json!({"vector": v, "top_k": 30, "boost": {"window": 30, "terms": terms},
                "include_attributes": ["g"]}),
    )
    .await;
    let unboosted = query(
        &api,
        &json!({"vector": v, "top_k": 30, "include_attributes": ["g"]}),
    )
    .await;
    assert_ne!(
        cut(&plain, 1, 4),
        cut(&unboosted, 1, 4),
        "the fixture proves nothing"
    );
    let got = query(
        &api,
        &json!({"vector": v, "top_k": 4, "boost": {"window": 30, "terms": terms},
                "limit_per": {"attributes": ["g"], "limit": 1}}),
    )
    .await;
    assert_eq!(ids(&got), cut(&plain, 1, 4));
    // A window only `limit_per` names is the boost's too: at the window of `top_k`, the cut
    // can only pick from the boosted first five.
    let five = query(
        &api,
        &json!({"vector": v, "top_k": 5, "boost": {"window": 5, "terms": terms},
                "include_attributes": ["g"]}),
    )
    .await;
    let got = query(
        &api,
        &json!({"vector": v, "top_k": 5, "boost": {"terms": terms},
                "limit_per": {"attributes": ["g"], "limit": 1, "window": 5}}),
    )
    .await;
    assert!(cut(&five, 1, 5).len() < 5, "the fixture proves nothing");
    assert_eq!(ids(&got), cut(&five, 1, 5));
}

#[tokio::test]
async fn what_cannot_mean_a_per_group_limit_is_refused_before_any_blob_request() {
    let store = Accounted::new(MemoryStore::new());
    let api = corpus(store.clone()).await;
    let spent =
        || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(22), c));
    let ok = json!({"attributes": ["g"], "limit": 1});
    let nine: Vec<String> = (0..9).map(|i| format!("a{i}")).collect();
    let v = json!([1.0, 0.0, 1.0, 0.5]);
    for body in [
        json!({"vector": v, "limit_per": {"attributes": ["g"], "limit": 0}}),
        json!({"vector": v, "limit_per": 2}),
        json!({"vector": v, "limit_per": {"limit": 1}}),
        json!({"vector": v, "limit_per": {"attributes": [], "limit": 1}}),
        json!({"vector": v, "limit_per": {"attributes": [1], "limit": 1}}),
        json!({"vector": v, "limit_per": {"attributes": ["g"], "limit": 1, "window": 1001}}),
        json!({"vector": v, "limit_per": {"attributes": ["g"], "limit": 1, "window": 20.5}}),
        json!({"vector": v, "top_k": 1001, "limit_per": ok}),
        json!({"vector": v, "limit_per": {"attributes": nine, "limit": 1}}),
        json!({"vector": v, "limit_per": {"attributes": ["g", "g"], "limit": 1}}),
        json!({"vector": v, "limit_per": {"attributes": ["g"]}}),
        json!({"vector": v, "limit_per": {"attributes": ["g"], "limit": 1, "x": 1}}),
        json!({"vector": v, "top_k": 50, "limit_per": {"attributes": ["g"], "limit": 1, "window": 20}}),
        json!({"rank_by": ["id", "asc"], "limit_per": ok}),
        json!({"aggregate_by": {"n": ["Count"]}, "limit_per": ok}),
        json!({"vector": v, "diversify": {"lambda": 0.5}, "limit_per": ok}),
        json!({"vector": v, "boost": {"window": 30, "terms": [{"attr": "pop", "fn": "linear", "weight": 0.01}]},
               "limit_per": {"attributes": ["g"], "limit": 1, "window": 40}}),
    ] {
        let before = spent();
        let (s, b) = send(&api, "POST", "/v1/indexes/docs/query", Some(&body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
        assert_eq!(b["error"]["code"], "bad_request", "{body} -> {b}");
        assert_eq!(before, spent(), "{body}: the refusal made blob requests");
    }
}

#[tokio::test]
async fn a_per_group_limit_costs_a_boosts_window() {
    let v = json!([1.0, 0.2, 1.0, 0.5]);
    let mut seen = Vec::new();
    for body in [
        json!({"vector": v, "top_k": 5, "boost": {"window": 30, "terms": [{"attr": "pop", "fn": "linear", "weight": 0.01}]}}),
        json!({"vector": v, "top_k": 5, "limit_per": {"attributes": ["g"], "limit": 1, "window": 30}}),
    ] {
        // A cold server each; clone shares the counters.
        let store = DepthCounting::new(MemoryStore::new());
        let api = corpus(Accounted::new(store.clone())).await;
        store.reset();
        query(&api, &body).await;
        assert!(
            store.depth() > 0 && store.requests() > 0,
            "nothing was measured"
        );
        seen.push((store.depth(), store.requests()));
    }
    assert_eq!(seen[0], seen[1], "boost, limit_per");
}

#[tokio::test]
async fn a_hybrid_query_is_cut_and_unfolded_hits_count_the_kept_rows() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    let v = json!([1.0, 0.2, 1.0, 0.5]);
    let hybrid = json!({"vector": v, "text": "common word2"});
    let with = |extra: Value| {
        let mut q = hybrid.clone();
        for (k, x) in extra.as_object().unwrap() {
            q[k] = x.clone();
        }
        q
    };
    let plain = query(
        &api,
        &with(json!({"top_k": 100, "include_attributes": ["g"]})),
    )
    .await;
    let got = query(
        &api,
        &with(json!({"top_k": 6, "limit_per": {"attributes": ["g"], "limit": 1}})),
    )
    .await;
    assert_eq!(ids(&got), cut(&plain, 1, 6));
    // Rewrites of ten folded ids, unfolded in the querying process.
    let docs: Vec<Value> = (0..10).map(doc).collect();
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"documents": docs})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let plain = query(
        &api,
        &json!({"vector": v, "top_k": 100, "include_attributes": ["g"]}),
    )
    .await;
    let got = query(
        &api,
        &json!({"vector": v, "top_k": 6, "limit_per": {"attributes": ["g"], "limit": 1}}),
    )
    .await;
    let kept = ids(&got);
    assert_eq!(kept, cut(&plain, 1, 6));
    let fresh = kept.iter().filter(|id| id.as_str() < "d10").count();
    assert!(
        fresh > 0 && fresh < kept.len(),
        "the fixture proves nothing: {kept:?}"
    );
    assert_eq!(got["meta"]["unfolded_hits"], fresh, "{got}");
}
