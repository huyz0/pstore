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

//! M66: geo filters and a geo decay, through the API, against a brute-force haversine written
//! here from the points the test wrote.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::BTreeMap;
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

fn haversine(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (p1, p2) = (a.0.to_radians(), b.0.to_radians());
    let dp = (b.0 - a.0).to_radians();
    let dl = (b.1 - a.1).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * h.sqrt().asin()
}

/// A deterministic spread of points over the globe, some within a degree of the antimeridian.
fn point(i: usize) -> (f64, f64) {
    let lat = ((i * 37 % 1700) as f64) / 10.0 - 85.0;
    let lon = if i.is_multiple_of(4) {
        // Straddling the antimeridian.
        179.0 + ((i * 13 % 20) as f64) / 10.0 - if i.is_multiple_of(8) { 360.0 } else { 0.0 }
    } else {
        ((i * 91 % 3500) as f64) / 10.0 - 175.0
    };
    (lat, lon.clamp(-180.0, 180.0))
}

fn vector(i: usize) -> Vec<f32> {
    (0..4).map(|j| ((i * 3 + j) as f32).sin()).collect()
}

async fn put<S: BlobStore + Clone + 'static>(
    api: &Arc<Api<S>>,
    rows: std::ops::Range<usize>,
    deletes: &[String],
) {
    let docs: Vec<Value> = rows
        .map(|i| {
            let (lat, lon) = point(i);
            json!({"id": format!("g{i:03}"), "vector": vector(i),
                   "attributes": {"loc": [lat, lon], "n": (i % 5) as i64}})
        })
        .collect();
    let (s, b) = send(
        api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": docs, "deletes": deletes})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold<S: BlobStore + Clone + 'static>(api: &Arc<Api<S>>) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// 200 folded rows (two segments), then unfolded upserts (a moved point) and deletes.
async fn corpus() -> (
    Arc<Api<MemoryStore>>,
    BTreeMap<String, (f64, f64)>,
    BTreeMap<String, i64>,
) {
    let api = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    put(&api, 0..100, &[]).await;
    fold(&api).await;
    put(&api, 100..200, &[]).await;
    fold(&api).await;
    // Unfolded: 20 deleted, and 10 upserted to a different point (i + 1000).
    let gone: Vec<String> = (0..20).map(|i| format!("g{:03}", i * 9)).collect();
    let moved: Vec<Value> = (0..10)
        .map(|k| {
            let (lat, lon) = point(1000 + k);
            json!({"id": format!("g{:03}", 100 + k), "vector": vector(100 + k),
                   "attributes": {"loc": [lat, lon], "n": ((100 + k) % 5) as i64}})
        })
        .collect();
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": moved})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // After the moves, so a deleted id stays deleted.
    put(&api, 200..210, &gone).await;
    let mut model = BTreeMap::new();
    let mut n = BTreeMap::new();
    for i in 0..210 {
        let id = format!("g{i:03}");
        let p = if (100..110).contains(&i) {
            point(1000 + (i - 100))
        } else {
            point(i)
        };
        model.insert(id.clone(), p);
        n.insert(id, (i % 5) as i64);
    }
    for g in &gone {
        model.remove(g);
        n.remove(g);
    }
    (api, model, n)
}

/// Whether the model keeps a row, given its id and point.
type Keep = Box<dyn Fn(&str, (f64, f64)) -> bool>;

fn ids(b: &Value) -> Vec<String> {
    let mut v: Vec<String> = b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    v.sort();
    v
}

fn admitted(
    model: &BTreeMap<String, (f64, f64)>,
    keep: impl Fn(&str, (f64, f64)) -> bool,
) -> Vec<String> {
    let mut v: Vec<String> = model
        .iter()
        .filter(|(id, p)| keep(id, **p))
        .map(|(id, p)| {
            let _ = p;
            id.clone()
        })
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn geo_filters_through_the_api() {
    let (api, model, n) = corpus().await;
    let center = (10.0, 20.0);
    let in_box = |p: (f64, f64), s: f64, w: f64, nn: f64, e: f64| {
        (s..=nn).contains(&p.0)
            && if w > e {
                p.1 >= w || p.1 <= e
            } else {
                (w..=e).contains(&p.1)
            }
    };
    let radius_m = 4_000_000.0;
    let filters: Vec<(Value, Keep)> = vec![
        (
            json!(["loc", "GeoRadius", [center.0, center.1, radius_m]]),
            Box::new(move |_, p| haversine(center, p) <= radius_m),
        ),
        (
            json!(["loc", "GeoBox", [-20.0, 10.0, 40.0, 100.0]]),
            Box::new(move |_, p| in_box(p, -20.0, 10.0, 40.0, 100.0)),
        ),
        (
            json!(["loc", "GeoBox", [-60.0, 170.0, 60.0, -170.0]]),
            Box::new(move |_, p| in_box(p, -60.0, 170.0, 60.0, -170.0)),
        ),
        (
            json!([
                "And",
                [
                    ["loc", "GeoRadius", [center.0, center.1, radius_m]],
                    ["n", "Eq", 2]
                ]
            ]),
            {
                let n = n.clone();
                Box::new(move |id, p| haversine(center, p) <= radius_m && n[id] == 2)
            },
        ),
        (
            json!(["Not", ["loc", "GeoBox", [-20.0, 10.0, 40.0, 100.0]]]),
            Box::new(move |_, p| !in_box(p, -20.0, 10.0, 40.0, 100.0)),
        ),
    ];
    for (filter, keep) in filters {
        let want = admitted(&model, &keep);
        assert!(
            !want.is_empty() && want.len() < model.len(),
            "{filter}: {}",
            want.len()
        );
        // A ranked query over all of them.
        let ranked = query(
            &api,
            &json!({"vector": vector(3), "exact": true, "top_k": 1000, "filters": filter}),
        )
        .await;
        assert_eq!(ids(&ranked), want, "ranked {filter}");
        // An order.
        let ordered = query(
            &api,
            &json!({"rank_by": ["n", "asc"], "top_k": 1000, "filters": filter}),
        )
        .await;
        assert_eq!(ids(&ordered), want, "ordered {filter}");
        // An aggregation.
        let counted = query(
            &api,
            &json!({"aggregate_by": {"c": ["Count", "id"]}, "filters": filter}),
        )
        .await;
        assert_eq!(counted["aggregations"]["c"], want.len(), "counted {filter}");
    }
}

#[tokio::test]
async fn geo_decay_scores_by_distance() {
    let (api, model, _) = corpus().await;
    let origin = (10.0, 20.0);
    let scale = 3_000_000.0;
    let base = query(
        &api,
        &json!({"vector": vector(3), "exact": true, "top_k": 1000}),
    )
    .await;
    let score = |b: &Value| -> BTreeMap<String, f64> {
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
    };
    let base = score(&base);
    assert_eq!(base.len(), model.len());
    for (shape, f) in [
        ("exp", (|r: f64| 0.5f64.powf(r)) as fn(f64) -> f64),
        ("gauss", |r| 0.5f64.powf(r * r)),
        ("linear", |r| (1.0 - r / 2.0).max(0.0)),
    ] {
        let boosted = query(
            &api,
            &json!({"vector": vector(3), "exact": true, "top_k": 1000, "boost": {"window": 1000,
                "terms": [{"attr": "loc", "fn": "decay", "weight": 1.0,
                           "origin": [origin.0, origin.1], "scale": scale, "shape": shape}]}}),
        )
        .await;
        let got = score(&boosted);
        assert_eq!(got.len(), base.len(), "{shape}");
        let mut moved = 0;
        for (id, s) in &got {
            let want = base[id] + f(haversine(origin, model[id]) / scale);
            assert!((s - want).abs() < 1e-4, "{shape} {id}: {s} vs {want}");
            if (s - base[id]).abs() > 1e-3 {
                moved += 1;
            }
        }
        assert!(moved > 10, "{shape}: only {moved} rows moved");
    }
    // At exactly `scale` meters every shape is one half.
    let probe = Api::new(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    let there = (0.0, 0.0);
    let d = 1_000_000.0;
    // A point `d` meters along the equator.
    let lon = (d / 6_371_008.8f64).to_degrees();
    let docs = [
        json!({"id": "near", "vector": [1.0, 0.0], "attributes": {"loc": [lon, 0.0]}}),
        json!({"id": "none", "vector": [1.0, 0.0], "attributes": {"loc": "paris"}}),
    ];
    let (s, b) = send(
        &probe,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&json!({"durability": "durable", "documents": docs})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let plain = score(&query(&probe, &json!({"vector": [1.0, 0.0], "exact": true})).await);
    for shape in ["exp", "gauss", "linear"] {
        let b = query(
            &probe,
            &json!({"vector": [1.0, 0.0], "exact": true, "boost": {"terms": [
                {"attr": "loc", "fn": "decay", "weight": 1.0, "origin": [there.0, there.1],
                 "scale": haversine((lon, 0.0), there), "shape": shape}]}}),
        )
        .await;
        let got = score(&b);
        assert!(
            (got["near"] - plain["near"] - 0.5).abs() < 1e-4,
            "{shape}: {got:?}"
        );
        // Not a location: contributes nothing.
        assert!((got["none"] - plain["none"]).abs() < 1e-6, "{shape}");
    }
}

#[tokio::test]
async fn geo_clauses_are_refused_at_the_door() {
    let (api, ..) = corpus().await;
    let filters = [
        (json!(["loc", "GeoRadius", [91.0, 0.0, 10.0]]), "GeoRadius"),
        (
            json!(["loc", "GeoRadius", [0.0, -181.0, 10.0]]),
            "GeoRadius",
        ),
        (json!(["loc", "GeoRadius", [0.0, 0.0, -1.0]]), "GeoRadius"),
        (
            json!(["loc", "GeoRadius", [0.0, 0.0, 20_015_088.0]]),
            "GeoRadius",
        ),
        (json!(["loc", "GeoRadius", [0.0, 0.0]]), "GeoRadius"),
        (json!(["loc", "GeoRadius", "paris"]), "GeoRadius"),
        (json!(["loc", "GeoRadius", [0.0, "x", 5.0]]), "GeoRadius"),
        (json!(["loc", "GeoBox", [10.0, 0.0, 5.0, 10.0]]), "GeoBox"),
        (json!(["loc", "GeoBox", [-91.0, 0.0, 5.0, 10.0]]), "GeoBox"),
        (json!(["loc", "GeoBox", [0.0, 0.0, 5.0, 181.0]]), "GeoBox"),
        (json!(["loc", "GeoBox", [0.0, 0.0, 5.0]]), "GeoBox"),
        (json!(["loc", "GeoBox", {"south": 0}]), "GeoBox"),
        (json!(["$id", "GeoBox", [0.0, 0.0, 5.0, 5.0]]), "reserved"),
    ];
    for (f, name) in filters {
        let why = refused(&api, &json!({"vector": vector(1), "filters": f})).await;
        assert!(why.contains(name), "{f} -> {why}");
    }
    let terms = [
        json!([{"attr": "loc", "fn": "decay", "weight": 1.0, "origin": [91.0, 0.0], "scale": 5}]),
        json!([{"attr": "loc", "fn": "decay", "weight": 1.0, "origin": [0.0, 0.0, 1.0], "scale": 5}]),
        json!([{"attr": "loc", "fn": "decay", "weight": 1.0, "origin": ["a", "b"], "scale": 5}]),
        json!([{"attr": "loc", "fn": "decay", "weight": 1.0, "origin": [0.0, 0.0], "scale": 0}]),
    ];
    for t in terms {
        let why = refused(&api, &json!({"vector": vector(1), "boost": {"terms": t}})).await;
        assert!(
            why.contains("origin") || why.contains("scale"),
            "{t} -> {why}"
        );
    }
    // The limits are accepted.
    for f in [
        json!(["loc", "GeoRadius", [90.0, -180.0, 0.0]]),
        json!(["loc", "GeoRadius", [-90, 180, 20_015_087]]),
        json!(["loc", "GeoBox", [-90.0, 180.0, 90.0, -180.0]]),
        json!(["loc", "GeoBox", [0.0, 0.0, 0.0, 0.0]]),
    ] {
        query(&api, &json!({"vector": vector(1), "filters": f})).await;
    }
}
