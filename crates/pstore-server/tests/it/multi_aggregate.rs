//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Aggregations and facets inside a multi-query through the API — M164. Every entry is
//! checked against the same sub-query run alone, and the counts against the rows written.
//! Criterion 3, the shared snapshot, is in `snapshot.rs` beside M131's.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_server::Api;
use pstore_testkit::depth::DepthCounting;
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
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
        .header("x-pstore-tenant", "23")
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

/// Forty documents: `n` is the row's number, `tag` one of four.
fn doc(i: usize) -> Value {
    #[allow(clippy::cast_precision_loss, reason = "i < 40")]
    let x = i as f32;
    json!({"id": format!("d{i:02}"), "vector": [x.cos(), x.sin(), 1.0, 0.5],
           "text": format!("common word{}", i % 5),
           "attributes": {"n": i, "tag": format!("t{}", i % 4)}})
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

const V: [f32; 4] = [1.0, 0.2, 1.0, 0.5];

fn ranked() -> Value {
    json!({"vector": V, "top_k": 3, "filters": ["n", "Gte", 10]})
}

fn totals() -> Value {
    json!({"aggregate_by": {"c": ["Count", "id"], "s": ["Sum", "n"]},
           "filters": ["n", "Lt", 30]})
}

fn grouped() -> Value {
    json!({"aggregate_by": {"c": ["Count", "id"]}, "group_by": ["tag"]})
}

fn faceted() -> Value {
    json!({"text": "word2", "top_k": 2,
           "facets": {"aggregate_by": {"c": ["Count", "id"]}, "group_by": ["tag"]}})
}

/// Each tag's count over the rows `keep` keeps, as M12's groups list them.
fn tag_counts(keep: impl Fn(usize) -> bool) -> Value {
    let groups: Vec<Value> = (0..4)
        .map(|t| {
            let c = (0..40).filter(|i| i % 4 == t && keep(*i)).count();
            json!({"tag": format!("t{t}"), "c": c})
        })
        .collect();
    json!(groups)
}

/// Criterion 1: every entry is the sub-query's own answer, and the totals are the rows'.
#[tokio::test]
async fn each_entry_is_the_sub_querys_own_answer() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    let subs = [ranked(), totals(), grouped(), faceted()];
    let b = query(&api, &json!({"queries": subs})).await;
    for (i, sub) in subs.iter().enumerate() {
        let alone = query(&api, sub).await;
        assert_eq!(b["results"][i], alone["results"], "{i}: {b}");
        for field in ["aggregations", "aggregation_groups", "facets"] {
            let want = alone.get(field).cloned().unwrap_or(Value::Null);
            assert_eq!(b[field][i], want, "{i} {field}: {b}");
        }
        assert_eq!(b["meta"]["facets_epochs"][i], alone["meta"]["facets_epoch"]);
        assert_eq!(
            b["meta"]["unfolded_hits"][i], alone["meta"]["unfolded_hits"],
            "{i}: {b}"
        );
    }
    // And the reference, from the rows written.
    let sum: usize = (0..30).sum();
    assert_eq!(b["aggregations"][1], json!({"c": 30, "s": sum}), "{b}");
    assert_eq!(b["aggregation_groups"][2], tag_counts(|_| true), "{b}");
    assert_eq!(b["facets"][3], tag_counts(|_| true), "{b}");
    assert_eq!(b["results"][0].as_array().unwrap().len(), 3, "{b}");
    assert_eq!(b["results"][3].as_array().unwrap().len(), 2, "{b}");
}

/// Criterion 2: no other window, and no outer limit, cuts what an aggregation counts.
#[tokio::test]
async fn no_rank_window_truncates_an_aggregation() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    let mut one = totals();
    one["top_k"] = json!(1);
    let all = json!({"aggregate_by": {"c": ["Count", "id"]}, "top_k": 1});
    let mut four = grouped();
    four["top_k"] = json!(4);
    let b = query(
        &api,
        &json!({"queries": [
            {"vector": V, "top_k": 2, "offset": 1},
            one,
            all,
            four,
        ]}),
    )
    .await;
    assert_eq!(b["results"][0].as_array().unwrap().len(), 2, "{b}");
    assert_eq!(b["aggregations"][1]["c"], 30, "{b}");
    assert_eq!(b["aggregations"][2]["c"], 40, "{b}");
    assert_eq!(b["aggregation_groups"][3], tag_counts(|_| true), "{b}");

    let b = query(
        &api,
        &json!({"queries": [
            {"vector": V, "top_k": 2, "filters": ["n", "Lt", 30],
             "facets": {"aggregate_by": {"c": ["Count", "id"]}, "group_by": ["tag"]}},
            {"text": "word2", "top_k": 2},
        ], "rerank_by": ["RRF"], "limit": 1}),
    )
    .await;
    assert_eq!(b["results"].as_array().unwrap().len(), 1, "{b}");
    assert_eq!(b["results"][0].as_array().unwrap().len(), 1, "{b}");
    assert_eq!(b["facets"], json!([tag_counts(|i| i < 30), null]), "{b}");
}

/// Criterion 4: the new fields appear only when a sub-query asks for them.
#[tokio::test]
async fn the_new_fields_appear_only_when_asked_for() {
    let api = corpus(Accounted::new(MemoryStore::new())).await;
    let keys = |b: &Value| {
        let root = ["aggregations", "aggregation_groups", "facets"].map(|k| b.get(k).is_some());
        (root, b["meta"].get("facets_epochs").is_some())
    };
    let b = query(&api, &json!({"queries": [ranked(), {"text": "word2"}]})).await;
    assert_eq!(keys(&b), ([false, false, false], false), "{b}");
    let b = query(&api, &json!({"queries": [ranked(), totals()]})).await;
    assert_eq!(keys(&b), ([true, true, false], false), "{b}");
    assert_eq!(b["aggregations"], json!([null, {"c": 30, "s": 435}]), "{b}");
    assert_eq!(b["aggregation_groups"], json!([null, null]), "{b}");
    assert_eq!(b["results"][1], json!([]), "{b}");
    assert_eq!(b["meta"]["unfolded_hits"][1], 0, "{b}");
    let b = query(&api, &json!({"queries": [ranked(), faceted()]})).await;
    assert_eq!(keys(&b), ([false, false, true], true), "{b}");
    assert_eq!(b["facets"][0], Value::Null, "{b}");
    assert_eq!(b["meta"]["facets_epochs"][0], Value::Null, "{b}");
    assert_eq!(b["meta"]["facets_epochs"][1], b["meta"]["epochs"][1], "{b}");
}

/// Criterion 5: aggregations and facets run beside the ranked legs, not after them.
#[tokio::test]
async fn aggregations_add_no_depth() {
    let measure = |body: Value| async move {
        // A cold server each, so no run reads what another warmed.
        let store = DepthCounting::new(MemoryStore::new());
        let api = corpus(Accounted::new(store.clone())).await;
        store.reset();
        query(&api, &body).await;
        assert!(store.depth() > 0, "nothing was measured: {body}");
        store.depth()
    };
    let subs = [ranked(), totals(), faceted()];
    let mut deepest = 0;
    for sub in &subs {
        deepest = deepest.max(measure(sub.clone()).await);
    }
    let multi = measure(json!({"queries": subs})).await;
    assert!(multi <= deepest, "multi {multi}, deepest alone {deepest}");
}

/// Criterion 6: what a sub-query cannot mean is refused before any blob request, with the
/// single query's reason.
#[tokio::test]
async fn what_cannot_mean_is_refused_before_any_blob_request() {
    let store = Accounted::new(MemoryStore::new());
    let api = corpus(store.clone()).await;
    let spent =
        || [OpClass::Read, OpClass::Write, OpClass::List].map(|c| store.count(TenantId(23), c));
    let agg = json!({"c": ["Count", "id"]});
    for (sub, why) in [
        (json!({"vector": V, "group_by": ["tag"]}), "group_by"),
        (json!({"aggregate_by": {"s": ["Sum", "id"]}}), "Sum"),
        (json!({"vector": V, "aggregate_by": agg}), "aggregate_by"),
        (json!({"facets": {"aggregate_by": agg}}), "facets"),
        (
            json!({"rank_by": ["n", "asc"], "facets": {"aggregate_by": agg}}),
            "facets",
        ),
    ] {
        for body in [sub.clone(), json!({"queries": [ranked(), sub]})] {
            let before = spent();
            let (s, b) = send(&api, "POST", "/v1/indexes/docs/query", Some(&body)).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{body} -> {b}");
            let msg = b["error"]["message"].as_str().unwrap();
            assert!(msg.contains(why), "{body}: {msg:?} does not name {why:?}");
            assert_eq!(before, spent(), "{body}: the refusal made blob requests");
        }
    }
    let body = json!({"queries": [ranked(), totals()], "rerank_by": ["RRF"]});
    let before = spent();
    let (s, b) = send(&api, "POST", "/v1/indexes/docs/query", Some(&body)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("rerank_by"),
        "{b}"
    );
    assert_eq!(before, spent(), "the refusal made blob requests");
}
