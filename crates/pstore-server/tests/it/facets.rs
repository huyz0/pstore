//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M65: an aggregation beside a ranked query, over the same filter.

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
        .header("x-pstore-tenant", "41")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn api() -> Arc<Api<MemoryStore>> {
    Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap()
}

async fn query<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>, q: &Value) -> Value {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", Some(q)).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b
}

async fn refused<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>, q: &Value) -> String {
    let (s, b) = send(api, "POST", "/v1/indexes/docs/query", Some(q)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{q} -> {b}");
    b.to_string()
}

async fn fold<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn vector(i: usize) -> Vec<f32> {
    (0..4)
        .map(|j| ((i * 5 + j * 3 + 1) as f32).sin() + 0.1 * j as f32)
        .collect()
}

const Q: [f32; 4] = [0.4, 0.9, -0.2, 0.3];

async fn write<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>, rows: std::ops::Range<usize>) {
    let docs: Vec<Value> = rows
        .map(|i| {
            let color = ["red", "green", "blue"][i % 3];
            json!({"id": format!("d{i:03}"), "vector": vector(i), "text": format!("alpha t{}", i % 3),
                   "attributes": {"color": color,
                                  "price": (i % 7) as i64}})
        })
        .collect();
    let (s, b) = send(
        api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": docs})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn delete<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>, id: &str) {
    let (s, b) = send(
        api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "deletes": [id]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Folded rows, then an upsert and a delete of folded ids left unfolded.
async fn corpus() -> Arc<Api<MemoryStore>> {
    let api = api();
    write(&api, 0..40).await;
    fold(&api).await;
    write(&api, 35..45).await;
    delete(&api, "d003").await;
    api
}

fn facets(group_by: bool) -> Value {
    if group_by {
        json!({"aggregate_by": {"n": ["Count", "id"], "total": ["Sum", "price"]},
               "group_by": ["color"], "top_k": 20})
    } else {
        json!({"aggregate_by": {"n": ["Count", "id"], "total": ["Sum", "price"]}})
    }
}

fn agg_of(b: &Value) -> Value {
    b.get("aggregation_groups")
        .map_or_else(|| b["aggregations"].clone(), Clone::clone)
}

#[tokio::test]
async fn facets_equal_the_aggregation_of_the_same_filter() {
    let api = corpus().await;
    let filters = [
        None,
        Some(json!(["price", "Lt", 4])),
        Some(json!(["color", "Eq", "red"])),
    ];
    for filter in filters {
        for grouped in [false, true] {
            let f = facets(grouped);
            let mut ranked = json!({"vector": Q, "exact": true, "top_k": 5, "facets": f});
            let mut alone = f.clone();
            if let Some(flt) = &filter {
                ranked["filters"] = flt.clone();
                alone["filters"] = flt.clone();
            }
            let want = query(&api, &alone).await;
            let got = query(&api, &ranked).await;
            assert_eq!(got["facets"], agg_of(&want), "{filter:?} grouped {grouped}");
            assert!(got["meta"]["facets_epoch"].is_u64(), "{got}");
        }
    }
    // At a past epoch, folded history only.
    let epoch =
        query(&api, &json!({"vector": Q, "top_k": 1, "exact": true})).await["meta"]["epoch"]
            .as_u64()
            .unwrap();
    fold(&api).await;
    write(&api, 100..110).await;
    fold(&api).await;
    let mut alone = facets(true);
    alone["as_of"] = json!(epoch);
    let want = query(&api, &alone).await;
    let got = query(
        &api,
        &json!({"vector": Q, "top_k": 3, "exact": true, "as_of": epoch, "facets": facets(true)}),
    )
    .await;
    assert_eq!(got["facets"], agg_of(&want));
}

#[tokio::test]
async fn facets_do_not_change_the_hits() {
    let api = corpus().await;
    let bodies = [
        json!({"vector": Q, "exact": true, "top_k": 6}),
        json!({"text": "alpha t1", "top_k": 6}),
        json!({"vector": Q, "exact": true, "text": "alpha t1", "top_k": 6}),
        json!({"vector": Q, "exact": true, "top_k": 6,
               "boost": {"terms": [{"attr": "price", "fn": "linear", "weight": 0.5}]}}),
        json!({"vector": Q, "exact": true, "top_k": 6,
               "diversify": {"lambda": 0.5, "window": 30}}),
        json!({"text": "alpha", "top_k": 6,
               "highlight": {"attr": "text", "pre": "<b>", "post": "</b>"}}),
    ];
    for body in bodies {
        let want = query(&api, &body).await;
        let mut with = body.clone();
        with["facets"] = facets(true);
        let got = query(&api, &with).await;
        assert_eq!(got["results"], want["results"], "{body}");
        assert!(
            got["facets"].is_array() || got["facets"].is_object(),
            "{got}"
        );
    }
}

#[tokio::test]
async fn facets_count_the_filtered_set_not_the_page() {
    let api = corpus().await;
    // 45 ids written, d003 deleted: 44 rows.
    let got = query(
        &api,
        &json!({"vector": Q, "exact": true, "top_k": 3,
                "diversify": {"lambda": 0.5, "window": 20},
                "facets": {"aggregate_by": {"n": ["Count", "id"]}}}),
    )
    .await;
    assert_eq!(got["results"].as_array().unwrap().len(), 3);
    assert_eq!(got["facets"]["n"], 44, "{got}");
    // Under a filter: price 0 is i % 7 == 0 among live ids.
    let want = (0..45).filter(|i| i % 7 == 0 && *i != 3).count();
    let got = query(
        &api,
        &json!({"vector": Q, "exact": true, "top_k": 2, "filters": ["price", "Eq", 0],
                "facets": {"aggregate_by": {"n": ["Count", "id"]}}}),
    )
    .await;
    assert_eq!(got["facets"]["n"], want, "{got}");
}

#[tokio::test]
async fn facets_ride_beside_the_ranking() {
    use pstore_testkit::depth::DepthCounting;
    for (segments, unfolded) in [(1, false), (1, true), (4, false), (4, true)] {
        let store = DepthCounting::new(MemoryStore::new());
        let cold = || Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
        let writer = cold();
        for k in 0..segments {
            write(&writer, k * 10..k * 10 + 10).await;
            fold(&writer).await;
        }
        let mut seen = Vec::new();
        let mut aggregation_only = facets(true);
        aggregation_only["filters"] = json!(["price", "Lt", 9]);
        for body in [
            json!({"vector": Q, "exact": true, "top_k": 5, "filters": ["price", "Lt", 9]}),
            aggregation_only.clone(),
            json!({"vector": Q, "exact": true, "top_k": 5, "filters": ["price", "Lt", 9],
                   "facets": facets(true)}),
        ] {
            let api = cold();
            if unfolded {
                write(&api, 3..8).await;
            }
            store.reset();
            let b = query(&api, &body).await;
            let c = &b["meta"]["cost"];
            seen.push((store.depth(), c["blob_reads"].as_u64().unwrap()));
        }
        // The deeper of the two, never their sum (4 + 3 = 7 if the facets ran after the ranking).
        assert!(
            seen[2].0 <= seen[0].0.max(seen[1].0) && seen[2].0 <= 4,
            "{segments} seg, unfolded {unfolded}: {seen:?}"
        );
        assert!(seen[2].1 <= seen[0].1 + seen[1].1, "{seen:?}");
        assert!(seen[2].1 > seen[0].1, "{seen:?}");
    }
}

#[tokio::test]
async fn facets_honour_the_consistency_level() {
    let store = MemoryStore::new();
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    write(&a, 0..10).await;
    fold(&a).await;
    // Another process with an unfolded durable write: a strong aggregation refuses (M12), and
    // so does a strong ranked query with facets.
    let w = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
    write(&w, 10..11).await;
    let b = Api::unsecured(Accounted::new(store.clone()), LaneId(3)).unwrap();
    let agg = json!({"n": ["Count", "id"]});
    let alone = json!({"aggregate_by": agg, "consistency": "strong"});
    let (alone_status, _) = send(&b, "POST", "/v1/indexes/docs/query", Some(&alone)).await;
    assert_eq!(alone_status, StatusCode::SERVICE_UNAVAILABLE);
    let ranked = json!({"vector": Q, "exact": true, "consistency": "strong",
                        "facets": {"aggregate_by": agg}});
    let (s, body) = send(&b, "POST", "/v1/indexes/docs/query", Some(&ranked)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    // Bounded: the facets are served, and staleness is reported.
    let c = Api::unsecured(Accounted::new(store.clone()), LaneId(4)).unwrap();
    let got = query(
        &c,
        &json!({"vector": Q, "exact": true, "consistency": "bounded", "max_staleness_ms": 60000,
                "facets": {"aggregate_by": agg}}),
    )
    .await;
    assert_eq!(got["facets"]["n"], 10, "{got}");
    assert!(got["meta"]["staleness_ms"].is_u64());
}

#[tokio::test]
async fn facets_are_refused_at_the_door() {
    let api = corpus().await;
    let agg = json!({"n": ["Count", "id"]});
    let cases = [
        json!({"vector": Q, "facets": 1}),
        json!({"vector": Q, "facets": {}}),
        json!({"vector": Q, "facets": {"aggregate_by": agg, "k": 1}}),
        json!({"vector": Q, "facets": {"aggregate_by": {}}}),
        json!({"vector": Q, "facets": {"aggregate_by": {"n": ["Sum", "id"]}}}),
        json!({"vector": Q, "facets": {"aggregate_by": {"color": ["Count", "id"]}, "group_by": ["color"]}}),
        json!({"vector": Q, "facets": {"aggregate_by": agg, "group_by": []}}),
        json!({"vector": Q, "facets": {"aggregate_by": agg, "top_k": 10001}}),
        json!({"vector": Q, "facets": {"aggregate_by": agg, "top_k": 0}}),
        json!({"facets": {"aggregate_by": agg}}),
        json!({"vector": Q, "rank_by": ["price", "asc"], "facets": {"aggregate_by": agg}}),
        json!({"vector": Q, "aggregate_by": agg, "facets": {"aggregate_by": agg}}),
        json!({"queries": [{"vector": Q, "facets": {"aggregate_by": agg}}]}),
    ];
    for q in cases {
        let why = refused(&api, &q).await;
        assert!(why.contains("facets"), "{q} -> {why}");
    }
    // The limits are accepted, and null is no facets.
    let ok = query(
        &api,
        &json!({"vector": Q, "top_k": 2, "facets": {"aggregate_by": agg, "top_k": 10000}}),
    )
    .await;
    assert_eq!(ok["facets"]["n"], 44);
    let none = query(&api, &json!({"vector": Q, "top_k": 2, "facets": null})).await;
    assert!(none.get("facets").is_none(), "{none}");
}
