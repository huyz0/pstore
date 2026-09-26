//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Datetime attributes through the API — M9h.3.

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
        .header("x-pstore-tenant", "29")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn put(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(
        api,
        request("PUT", "/v1/indexes/docs/documents", Some(body)),
    )
    .await
}

async fn fold(api: &Arc<Api<MemoryStore>>) {
    let (s, b) = send(api, request("POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn query(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(api, request("POST", "/v1/indexes/docs/query", Some(body))).await
}

fn doc(id: &str, attrs: Value) -> Value {
    json!({"id": id, "vector": [1.0, 0.5], "attributes": attrs})
}

async fn admitted(api: &Arc<Api<MemoryStore>>, filter: Value) -> BTreeSet<String> {
    let q = json!({"vector": [1.0, 0.5], "top_k": 100, "filters": filter});
    let (s, b) = query(api, &q).await;
    assert_eq!(s, StatusCode::OK, "{q} -> {b}");
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

fn declared(documents: Value) -> Value {
    json!({"durability": "durable", "schema": {"t": "datetime"}, "documents": documents})
}

async fn attrs_of(api: &Arc<Api<MemoryStore>>) -> Value {
    let q = json!({"vector": [1.0, 0.5], "top_k": 100, "include_attributes": true});
    let (s, b) = query(api, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut out = serde_json::Map::new();
    for row in b["results"].as_array().unwrap() {
        out.insert(
            row["id"].as_str().unwrap().to_owned(),
            row["attributes"].clone(),
        );
    }
    Value::Object(out)
}

#[tokio::test]
async fn datetimes_come_back_in_utc() {
    let cases = [
        (
            "a",
            json!("2024-05-01T12:30:00Z"),
            json!("2024-05-01T12:30:00Z"),
        ),
        (
            "b",
            json!("2024-05-01T14:30:00+02:00"),
            json!("2024-05-01T12:30:00Z"),
        ),
        (
            "c",
            json!("1999-12-31T23:59:59.123456Z"),
            json!("1999-12-31T23:59:59.123456Z"),
        ),
        (
            "d",
            json!("2024-05-01T12:30:00.500Z"),
            json!("2024-05-01T12:30:00.5Z"),
        ),
        (
            "e",
            json!(["2024-01-01T00:00:00Z"]),
            json!(["2024-01-01T00:00:00Z"]),
        ),
        (
            "f",
            json!("2024-02-29T00:00:00Z"),
            json!("2024-02-29T00:00:00Z"),
        ),
        (
            "g",
            json!("2000-02-29T00:00:00-00:00"),
            json!("2000-02-29T00:00:00Z"),
        ),
        ("h", json!([]), json!([])),
        (
            "i",
            json!("0001-01-01T00:00:00Z"),
            json!("0001-01-01T00:00:00Z"),
        ),
    ];
    for folded in [false, true] {
        let api = api();
        let documents: Vec<Value> = cases
            .iter()
            .map(|(id, v, _)| doc(id, json!({"t": v})))
            .collect();
        let (s, b) = put(&api, &declared(json!(documents))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // Undeclared, the same string stays a string, written as it was.
        let plain = json!({"documents": [doc("p", json!({"t": "2024-05-01T14:30:00+02:00"}))]});
        let (s, b) = put(&api, &plain).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        if folded {
            fold(&api).await;
        }
        let got = attrs_of(&api).await;
        for (id, _, want) in &cases {
            assert_eq!(got[id]["t"], *want, "folded {folded}: {id}");
        }
        assert_eq!(got["p"]["t"], "2024-05-01T14:30:00+02:00");
    }
}

#[tokio::test]
async fn a_string_literal_meets_a_datetime_by_instant_and_a_string_by_bytes() {
    for folded in [false, true] {
        let api = api();
        let dated = json!([
            doc("a", json!({"t": "2024-01-01T00:00:00Z"})),
            doc("b", json!({"t": "2024-06-01T00:00:00Z"})),
            doc("c", json!({"t": "2025-01-01T00:00:00Z"})),
            doc("arr", json!({"t": ["2024-01-01T00:00:00Z"]})),
        ]);
        let (s, b) = put(&api, &declared(dated)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let plain = json!({"durability": "durable", "documents": [
            doc("s", json!({"t": "2024-06-01T00:00:00Z"})),
            doc("n", json!({"t": 5})),
        ]});
        let (s, b) = put(&api, &plain).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        if folded {
            fold(&api).await;
        }
        let is = |f: Value, want: &[&str]| {
            let api = Arc::clone(&api);
            let want = set(want);
            async move {
                assert_eq!(
                    admitted(&api, f.clone()).await,
                    want,
                    "folded {folded}: {f}"
                );
            }
        };
        is(
            json!(["t", "Gte", "2024-06-01T00:00:00Z"]),
            &["b", "c", "s"],
        )
        .await;
        is(json!(["t", "Lt", "2024-03-01T01:00:00+01:00"]), &["a"]).await;
        is(json!(["t", "Eq", "2024-06-01T02:00:00+02:00"]), &["b"]).await;
        is(json!(["t", "Gt", "not a date"]), &[]).await;
        is(json!(["t", "Eq", 5]), &["n"]).await;
        is(json!(["t", "In", ["2024-01-01T00:00:00Z"]]), &["a"]).await;
        is(
            json!(["t", "Contains", "2024-01-01T01:00:00+01:00"]),
            &["arr"],
        )
        .await;
    }
}

#[tokio::test]
async fn a_datetime_column_prunes_its_blocks() {
    let api = api();
    let base = 1_704_067_200_i64; // 2024-01-01T00:00:00Z
    let documents: Vec<Value> = (0..2000)
        .map(|i| {
            let secs = base + i64::from(i) * 3600;
            doc(&format!("d{i:05}"), json!({"t": rfc3339(secs)}))
        })
        .collect();
    let (s, b) = put(&api, &declared(json!(documents))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(&api).await;
    let bytes = |f: Value| {
        let api = Arc::clone(&api);
        async move {
            let q = json!({"rank_by": ["t", "asc"], "top_k": 10, "filters": f});
            let (s, b) = query(&api, &q).await;
            assert_eq!(s, StatusCode::OK, "{q} -> {b}");
            assert_eq!(b["results"][0]["id"], "d00000");
            b["meta"]["cost"]["bytes_read"].as_u64().unwrap()
        }
    };
    let cut = "2024-01-03T16:00:00Z";
    let unpruned = bytes(json!(["Not", ["t", "Gte", cut]])).await;
    let pruned = bytes(json!(["t", "Lt", cut])).await;
    assert!(
        pruned * 4 <= unpruned,
        "{pruned} bytes against {unpruned} unpruned"
    );
}

/// Seconds since the epoch as RFC 3339, UTC, for dates after 1970 (the test's own civil
/// calendar, independent of the server's).
fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (mut y, mut d) = (1970_i64, days);
    loop {
        let len = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            366
        } else {
            365
        };
        if d < len {
            break;
        }
        d -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    while d >= months[m] {
        d -= months[m];
        m += 1;
    }
    format!(
        "{y:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        m + 1,
        d + 1,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[tokio::test]
async fn rank_by_puts_datetimes_between_numbers_and_strings() {
    let api = api();
    let (s, b) = put(
        &api,
        &declared(json!([
            // Instant order is the reverse of text order: 19:00 UTC, then 20:00.
            doc("d24", json!({"t": "2024-01-01T00:00:00+05:00"})),
            doc("d23", json!({"t": "2023-12-31T20:00:00Z"})),
        ])),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let others = json!({"durability": "durable", "documents": [
        doc("t", json!({"t": true})),
        doc("i", json!({"t": 1})),
        doc("s", json!({"t": "a"})),
        doc("arr", json!({"t": [1]})),
        doc("none", json!({})),
    ]});
    let (s, b) = put(&api, &others).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for folded in [false, true] {
        if folded {
            fold(&api).await;
        }
        let order = |dir: &'static str| {
            let api = Arc::clone(&api);
            async move {
                let q = json!({"rank_by": ["t", dir], "top_k": 100});
                let (s, b) = query(&api, &q).await;
                assert_eq!(s, StatusCode::OK, "{b}");
                b["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["id"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(
            order("asc").await,
            ["t", "i", "d24", "d23", "s", "arr", "none"],
            "folded {folded}"
        );
        assert_eq!(
            order("desc").await,
            ["arr", "s", "d23", "d24", "i", "t", "none"],
            "folded {folded}"
        );
    }
}

#[tokio::test]
async fn what_is_not_a_datetime_is_refused() {
    for v in [
        json!("2024-05-01"),
        json!("2024-05-01T12:30:00.1234567Z"),
        json!("2024-05-01T12:30:00.Z"),
        json!("2024-05-01 12:30:00Z"),
        json!("2024-05-01t12:30:00z"),
        json!("2024-05-01T12:30:60Z"),
        json!("2024-05-01T12:30:00+24:00"),
        json!("2024-05-01T12:30:00+01:60"),
        json!("2024-13-01T00:00:00Z"),
        json!("2023-02-29T00:00:00Z"),
        json!("1900-02-29T00:00:00Z"),
        json!("9999-12-31T23:59:59-01:00"),
        json!("0000-01-01T00:00:00+01:00"),
        json!(1),
        json!(null),
        json!(["2024-05-01"]),
    ] {
        let api = api();
        let (s, b) = put(&api, &declared(json!([doc("a", json!({"t": v}))]))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v} was stored: {b}");
    }
    for schema in [
        json!({"t": "int"}),
        json!({"t": "[]datetime"}),
        json!({"unused": "datetime"}),
        json!({"text": "datetime"}),
        json!(["t"]),
    ] {
        let api = api();
        let body = json!({"schema": schema, "documents": [
            {"id": "a", "vector": [1.0, 0.5], "text": "2024-05-01T12:30:00Z",
             "attributes": {"t": "2024-05-01T12:30:00Z"}}
        ]});
        let (s, b) = put(&api, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{schema} was accepted: {b}");
    }
}
