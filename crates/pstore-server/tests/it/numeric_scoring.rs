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

//! M203: `Attribute`, numbers, `Saturate`, `Decay` and `Dist` in `rank_by`, over HTTP.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "203")
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

async fn query(api: &A, rank_by: &Value) -> (StatusCode, Value) {
    send(
        api,
        "POST",
        "/v1/indexes/s/query",
        &json!({"rank_by": rank_by, "top_k": 100}),
    )
    .await
}

/// The answer's scores by id.
async fn scores(api: &A, rank_by: &Value) -> BTreeMap<String, f64> {
    let (s, b) = query(api, rank_by).await;
    assert_eq!(s, StatusCode::OK, "{rank_by}: {b}");
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
}

/// `(title, clicks, negative reviews, published hours after 2026-02-03T12:13:14Z)`; `None`
/// is a missing attribute.
/// A row: its title and three attributes.
type Row = (&'static str, Option<i64>, Option<i64>, Option<i64>);

const DOCS: [Row; 6] = [
    ("the quick fox", Some(100), Some(0), Some(0)),
    ("a fox", Some(300), Some(2), Some(-6)),
    ("quick brown dogs", Some(0), Some(6), Some(12)),
    ("quick fox jumps", None, None, None),
    ("nothing", Some(-50), Some(-1), Some(3)),
    ("fox", Some(25), Some(1), Some(48)),
];

/// 2026-02-03T12:13:14Z, in milliseconds.
const ORIGIN_MS: i64 = 1_770_120_794_000;

fn instant(hours: i64) -> String {
    let secs = ORIGIN_MS / 1000 + hours * 3600;
    // Days from the epoch to a civil date, the inverse of `days_from_civil`.
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
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

async fn index() -> A {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    let docs: Vec<Value> = DOCS
        .iter()
        .enumerate()
        .map(|(i, (title, clicks, negative, published))| {
            let mut attrs = json!({"title": title});
            if let Some(c) = clicks {
                attrs["clicks"] = json!(c);
            }
            if let Some(n) = negative {
                attrs["number_of_negative_reviews"] = json!(n);
            }
            if let Some(h) = published {
                attrs["published_at"] = json!(instant(*h));
            }
            json!({"id": format!("d{i}"), "vector": [1.0, i as f32], "attributes": attrs})
        })
        .collect();
    let schema = json!({
        "title": {"type": "string", "full_text_search": true},
        "published_at": "datetime",
    });
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/s/documents",
        &json!({"durability": "durable", "schema": schema, "documents": docs}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    api
}

/// The BM25 of `"quick fox"` over `title`, by id, from a lone clause (M182).
async fn bm25(api: &A) -> BTreeMap<String, f64> {
    scores(api, &json!(["title", "BM25", "quick fox"])).await
}

fn assert_close(got: &BTreeMap<String, f64>, want: &BTreeMap<String, f64>, what: &str) {
    let got_ids: Vec<&String> = got.keys().collect();
    let want_ids: Vec<&String> = want
        .iter()
        .filter(|(_, s)| **s > 0.0)
        .map(|(id, _)| id)
        .collect();
    assert_eq!(got_ids, want_ids, "{what}: {got:?} against {want:?}");
    for (id, s) in got {
        let w = want[id];
        assert!((s - w).abs() <= 1e-5 * w, "{what}: {id} {s} against {w}");
    }
}

/// Criterion 4: the vendor's three examples, each as its definition computes it.
#[tokio::test]
async fn numeric_expressions_over_http() {
    let api = index().await;
    let text = bm25(&api).await;
    let ids = || (0..DOCS.len()).map(|i| format!("d{i}"));

    // Saturate of clicks, midpoint 100, under a weight of 1.7.
    let got = scores(
        &api,
        &json!(["Sum", [["title", "BM25", "quick fox"],
                        ["Product", 1.7, ["Saturate", ["Attribute", "clicks"], {"midpoint": 100}]]]]),
    )
    .await;
    let want: BTreeMap<String, f64> = ids()
        .zip(DOCS)
        .map(|(id, (_, clicks, _, _))| {
            let x = clicks.map_or(0.0, |c| (c as f64).max(0.0));
            let s = text.get(&id).copied().unwrap_or(0.0) + 1.7 * x / (x + 100.0);
            (id, s)
        })
        .collect();
    assert_close(&got, &want, "Saturate");

    // Decay of negative reviews, midpoint 2; and an exponent of 2.
    for exponent in [None, Some(2.0)] {
        let mut params = json!({"midpoint": 2});
        if let Some(p) = exponent {
            params["exponent"] = json!(p);
        }
        let got = scores(
            &api,
            &json!([
                "Sum",
                [
                    ["title", "BM25", "quick fox"],
                    ["Decay", ["Attribute", "number_of_negative_reviews"], params]
                ]
            ]),
        )
        .await;
        let p = exponent.unwrap_or(1.0);
        let want: BTreeMap<String, f64> = ids()
            .zip(DOCS)
            .map(|(id, (_, _, negative, _))| {
                let decay = negative.map_or(0.0, |n| {
                    let x = (n as f64).max(0.0);
                    2f64.powf(p) / (x.powf(p) + 2f64.powf(p))
                });
                let s = text.get(&id).copied().unwrap_or(0.0) + decay;
                (id, s)
            })
            .collect();
        assert_close(&got, &want, &format!("Decay, exponent {exponent:?}"));
    }

    // Decay of the distance from an instant, midpoint 6h: the vendor's spelling, with no
    // offset, and the same instant in UTC and as milliseconds agree.
    let recency = |origin: &str, midpoint: Value| {
        json!(["Sum", [["title", "BM25", "quick fox"],
                       ["Decay", ["Dist", ["Attribute", "published_at"], origin],
                        {"midpoint": midpoint}]]])
    };
    let got = scores(&api, &recency("2026-02-03T12:13:14", json!("6h"))).await;
    let want: BTreeMap<String, f64> = ids()
        .zip(DOCS)
        .map(|(id, (_, _, _, published))| {
            let decay = published.map_or(0.0, |h| {
                let x = (h as f64 * 3_600_000.0).abs();
                let m = 6.0 * 3_600_000.0;
                m / (x + m)
            });
            let s = text.get(&id).copied().unwrap_or(0.0) + decay;
            (id, s)
        })
        .collect();
    assert_close(&got, &want, "Dist");
    for (origin, midpoint) in [
        ("2026-02-03T12:13:14Z", json!("6h")),
        ("2026-02-03T13:13:14+01:00", json!(21_600_000)),
        ("2026-02-03T12:13:14", json!("360m")),
        ("2026-02-03T12:13:14", json!("0.25d")),
    ] {
        assert_eq!(
            scores(&api, &recency(origin, midpoint.clone())).await,
            got,
            "{origin} {midpoint}"
        );
    }
    // A unit is not ignored.
    assert_ne!(
        scores(&api, &recency("2026-02-03T12:13:14", json!("6d"))).await,
        got
    );

    // A number clause, and Max with 0: the rows whose clicks are above 0, by clicks.
    let got = scores(&api, &json!(["Max", [0, ["Attribute", "clicks"]]])).await;
    let want: BTreeMap<String, f64> = [("d0", 100.0), ("d1", 300.0), ("d5", 25.0)]
        .into_iter()
        .map(|(id, s)| (id.to_owned(), s))
        .collect();
    assert_eq!(got, want);
    // Dist from a number.
    let got = scores(&api, &json!(["Dist", ["Attribute", "clicks"], 100])).await;
    assert_eq!(got.get("d1"), Some(&200.0));
    assert_eq!(got.get("d4"), Some(&150.0));
    assert!(!got.contains_key("d0"), "a distance of 0 is dropped");

    // An attribute named Attribute is still ordered by; a filter on one named Dist is a filter.
    let (s, b) = query(&api, &json!(["Attribute", "asc"])).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(
        b["results"][0].get("score").is_none_or(Value::is_null),
        "an order: {b}"
    );
    let (s, b) = query(&api, &json!(["Sum", [["Dist", "Eq", 3], 1]])).await;
    assert_eq!(s, StatusCode::OK, "a filter on Dist: {b}");
    let (s, b) = query(&api, &json!(["Sum", [["Attribute", "Eq", 3], 1]])).await;
    assert_eq!(s, StatusCode::OK, "a filter on Attribute: {b}");
}

/// Criterion 4: every refusal in Delta 7 is a 400.
#[tokio::test]
async fn numeric_expressions_out_of_bounds_are_refused() {
    let api = index().await;
    let attr = || json!(["Attribute", "clicks"]);
    let sat = |params: Value| json!(["Saturate", attr(), params]);
    for bad in [
        // Signed roots.
        attr(),
        json!(["Max", [-1]]),
        json!(["Sum", [["title", "BM25", "fox"], attr()]]),
        json!(["Product", 2, attr()]),
        json!(["Max", [attr(), -1]]),
        // Numbers out of range.
        json!(["Sum", [1e31]]),
        json!(["Dist", attr(), 1e31]),
        // Midpoints and exponents.
        sat(json!({})),
        sat(json!({"midpoint": 0})),
        sat(json!({"midpoint": -1})),
        sat(json!({"midpoint": 1e31})),
        sat(json!({"midpoint": "6"})),
        sat(json!({"midpoint": "6y"})),
        sat(json!({"midpoint": "0s"})),
        sat(json!({"midpoint": "h"})),
        sat(json!({"midpoint": "1.h"})),
        sat(json!({"midpoint": ".5h"})),
        sat(json!({"midpoint": "1234567890123456s"})),
        sat(json!({"midpoint": true})),
        sat(json!({"midpoint": 1, "exponent": 0})),
        sat(json!({"midpoint": 1, "exponent": 17})),
        sat(json!({"midpoint": 1, "exponent": "2"})),
        sat(json!({"midpoint": 1, "pivot": 2})),
        json!(["Decay", attr()]),
        json!(["Decay", attr(), 3]),
        json!(["Saturate", attr(), {"midpoint": 1}, 2]),
        // Dist's arguments.
        json!(["Dist", ["Sum", [1]], 3]),
        json!(["Dist", attr(), true]),
        json!(["Dist", attr(), "yesterday"]),
        json!(["Dist", attr()]),
        // Attribute's.
        json!(["Max", [0, ["Attribute"]]]),
        json!(["Max", [0, ["Attribute", 3]]]),
        json!(["Max", [0, ["Attribute", "clicks", "x"]]]),
    ] {
        let (s, b) = query(&api, &bad).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {b}");
    }
}
