//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! UUID and bytes attributes through the API (M171): declared by a write, stored as what
//! they are, answered in one spelling, filtered, ordered and boosted by their values.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

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
        .header("x-pstore-tenant", "23")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn write(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(api, request("PUT", "/v1/indexes/u/documents", Some(body))).await
}

async fn query(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(api, request("POST", "/v1/indexes/u/query", Some(body))).await
}

async fn fold(api: &Arc<Api<MemoryStore>>) {
    let (s, b) = send(api, request("POST", "/v1/admin/fold", None)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn api(store: &Accounted<MemoryStore>) -> Arc<Api<MemoryStore>> {
    Api::unsecured(store.clone(), LaneId(1)).unwrap()
}

fn ids(b: &Value) -> Vec<String> {
    b["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results: {b}"))
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

const U1: &str = "00000000-0000-4000-8000-000000000001";
const U2: &str = "6f1c2a3b-4d5e-4f60-8a7b-9c8d7e6f5a4b";
const U3: &str = "ffffffff-0000-4000-8000-000000000000";

/// Four rows: `r` a UUID, `b` bytes, both arrays too; `s` a string spelling U1 as text.
fn rows() -> Value {
    json!([
        {"id": "a", "vector": [1.0, 0.0],
         "attributes": {"r": U1, "b": "AAE=", "rs": [U1, U3], "bs": ["AA==", "/w=="]}},
        {"id": "b", "vector": [1.0, 0.0],
         "attributes": {"r": U2.to_uppercase(), "b": "AAI=", "rs": [U2], "bs": ["AQ=="]}},
        {"id": "c", "vector": [1.0, 0.0],
         "attributes": {"r": U3, "b": "/////w==", "rs": [], "bs": []}},
        {"id": "s", "vector": [1.0, 0.0], "attributes": {"t": U1}},
    ])
}

fn declared() -> Value {
    json!({"r": "uuid", "rs": "uuid", "b": "bytes", "bs": "bytes"})
}

async fn seeded(store: &Accounted<MemoryStore>, durable: bool) -> Arc<Api<MemoryStore>> {
    let a = api(store);
    let mut body = json!({"documents": rows(), "schema": declared()});
    if durable {
        body["durability"] = json!("durable");
    }
    let (s, b) = write(&a, &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    if durable {
        fold(&a).await;
    }
    a
}

async fn projected(a: &Arc<Api<MemoryStore>>) -> Value {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 10, "include_attributes": true});
    let (s, b) = query(a, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn assert_spelled(b: &Value) {
    let r = &b["results"];
    assert_eq!(r[0]["attributes"]["r"], U1, "{b}");
    assert_eq!(r[1]["attributes"]["r"], U2, "lowercased: {b}");
    assert_eq!(r[0]["attributes"]["b"], "AAE=", "{b}");
    assert_eq!(r[2]["attributes"]["b"], "/////w==", "{b}");
    assert_eq!(r[0]["attributes"]["rs"], json!([U1, U3]), "{b}");
    assert_eq!(r[0]["attributes"]["bs"], json!(["AA==", "/w=="]), "{b}");
    assert_eq!(r[3]["attributes"]["t"], U1, "a string stays one: {b}");
}

#[tokio::test]
async fn uuids_and_bytes_answer_as_written_before_and_after_a_fold_and_a_restart() {
    let store = Accounted::new(MemoryStore::new());
    let unfolded = seeded(&store, false).await;
    assert_spelled(&projected(&unfolded).await);

    let store = Accounted::new(MemoryStore::new());
    let a = seeded(&store, true).await;
    assert_spelled(&projected(&a).await);
    drop(a);
    assert_spelled(&projected(&api(&store)).await);
}

#[tokio::test]
async fn filters_read_a_string_literal_as_the_rows_type() {
    let store = Accounted::new(MemoryStore::new());
    let a = seeded(&store, true).await;
    let is = |f: Value, want: &'static [&'static str]| {
        let a = Arc::clone(&a);
        async move {
            let q = json!({"rank_by": ["id", "asc"], "top_k": 10, "filters": f});
            let (s, b) = query(&a, &q).await;
            assert_eq!(s, StatusCode::OK, "{f}: {b}");
            assert_eq!(ids(&b), want, "{f}: {b}");
        }
    };
    // Byte order is the lowercase text's order: U1 < U2 < U3.
    is(json!(["r", "Eq", U1]), &["a"]).await;
    is(json!(["r", "Eq", U2.to_uppercase()]), &["b"]).await;
    is(json!(["r", "NotEq", U1]), &["b", "c", "s"]).await;
    is(json!(["r", "In", [U1, U3]]), &["a", "c"]).await;
    is(json!(["r", "Lt", U2]), &["a"]).await;
    is(json!(["r", "Gte", U2]), &["b", "c"]).await;
    is(json!(["rs", "ContainsAny", [U3, U2]]), &["a", "b"]).await;
    // An unparseable literal equals no UUID row, so its negation admits every one.
    is(json!(["r", "Eq", "not-a-uuid"]), &[]).await;
    is(json!(["r", "NotEq", "not-a-uuid"]), &["a", "b", "c", "s"]).await;
    // A UUID row and a string row of the same text are told apart.
    is(json!(["t", "Eq", U1]), &["s"]).await;
    // Bytes compare bytewise: 00 01 < 00 02 < ff ff ff ff.
    is(json!(["b", "Eq", "AAI="]), &["b"]).await;
    is(json!(["b", "Gt", "AAE="]), &["b", "c"]).await;
    is(json!(["b", "Lte", "AAI="]), &["a", "b"]).await;
    is(json!(["bs", "ContainsAny", ["/w=="]]), &["a"]).await;
    is(json!(["b", "Eq", "AAE"]), &[]).await;
}

#[tokio::test]
async fn rank_by_orders_uuids_then_bytes_after_strings_and_before_arrays() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store);
    let docs = json!([
        {"id": "n", "vector": [1.0, 0.0], "attributes": {"k": 5}},
        {"id": "s", "vector": [1.0, 0.0], "attributes": {"k": "zzz"}},
        {"id": "u2", "vector": [1.0, 0.0], "attributes": {"k": U2}},
        {"id": "u1", "vector": [1.0, 0.0], "attributes": {"k": U1}},
        {"id": "y", "vector": [1.0, 0.0], "attributes": {"k": "AA=="}},
        {"id": "y2", "vector": [1.0, 0.0], "attributes": {"k": "/w=="}},
        {"id": "l", "vector": [1.0, 0.0], "attributes": {"k": [1]}},
        {"id": "none", "vector": [1.0, 0.0]},
    ]);
    // One name, three declarations is not a thing a write can say: two writes.
    let typed: Vec<Value> = docs.as_array().unwrap().clone();
    let (uuids, rest): (Vec<Value>, Vec<Value>) = typed
        .into_iter()
        .partition(|d| d["id"].as_str().unwrap().starts_with('u'));
    let (bytes, plain): (Vec<Value>, Vec<Value>) = rest
        .into_iter()
        .partition(|d| d["id"].as_str().unwrap().starts_with('y'));
    for (docs, schema) in [
        (uuids, json!({"k": "uuid"})),
        (bytes, json!({"k": "bytes"})),
        (plain, Value::Null),
    ] {
        let mut body = json!({"durability": "durable", "documents": docs});
        if !schema.is_null() {
            body["schema"] = schema;
        }
        let (s, b) = write(&a, &body).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    fold(&a).await;
    let order = |dir: &str| json!({"rank_by": ["k", dir], "top_k": 10});
    let (s, b) = query(&a, &order("asc")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        ids(&b),
        ["n", "s", "u1", "u2", "y", "y2", "l", "none"],
        "{b}"
    );
    let (s, b) = query(&a, &order("desc")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        ids(&b),
        ["l", "y2", "y", "u2", "u1", "s", "n", "none"],
        "{b}"
    );
}

#[tokio::test]
async fn a_boost_and_a_ref_new_condition_read_uuids_as_filters_do() {
    let store = Accounted::new(MemoryStore::new());
    let a = seeded(&store, true).await;
    let t = json!({"attr": "r", "fn": "eq", "weight": 100.0, "value": U3});
    let q = json!({"vector": [1.0, 0.0], "top_k": 4, "boost": {"terms": [t]}});
    let (s, b) = query(&a, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ids(&b)[0], "c", "the boosted row first: {b}");
    let t = json!({"attr": "b", "fn": "eq", "weight": 100.0, "value": "AAI="});
    let q = json!({"vector": [1.0, 0.0], "top_k": 4, "boost": {"terms": [t]}});
    let (s, b) = query(&a, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ids(&b)[0], "b", "the boosted bytes row first: {b}");

    // Replaces `a` only when the stored UUID equals the new one.
    for (r, applies) in [(U2, false), (U1, true)] {
        let body = json!({"durability": "durable", "schema": {"r": "uuid"},
            "documents": [{"id": "a", "vector": [1.0, 0.0], "attributes": {"r": r, "n": 1}}],
            "upsert_condition": ["r", "Eq", {"$ref_new": "r"}]});
        let (s, b) = write(&a, &body).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        fold(&a).await;
        let (s, b) = query(
            &a,
            &json!({"rank_by": ["id", "asc"], "top_k": 1,
                                       "include_attributes": ["n"]}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let n = &b["results"][0]["attributes"]["n"];
        assert_eq!(n == &json!(1), applies, "{r}: {b}");
    }
    // And over bytes: `b` holds `AAI=`, which no upsert above touched.
    for (y, applies) in [("AAE=", false), ("AAI=", true)] {
        let body = json!({"durability": "durable", "schema": {"b": "bytes"},
            "documents": [{"id": "b", "vector": [1.0, 0.0], "attributes": {"b": y, "m": 1}}],
            "upsert_condition": ["b", "Eq", {"$ref_new": "b"}]});
        let (s, b) = write(&a, &body).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        fold(&a).await;
        let only_b = json!({"rank_by": ["id", "asc"], "top_k": 1,
                            "filters": ["id", "Eq", "b"], "include_attributes": ["m"]});
        let (s, b) = query(&a, &only_b).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let m = &b["results"][0]["attributes"]["m"];
        assert_eq!(m == &json!(1), applies, "{y}: {b}");
    }
}

#[tokio::test]
async fn malformed_typed_values_are_refused_and_nothing_is_buffered() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store);
    let big = "A".repeat(4100 / 3 * 4 + 4);
    let at_most = {
        // 4,096 bytes exactly is the bound, and is taken.
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(vec![7u8; 4096])
    };
    let over = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(vec![7u8; 4097])
    };
    for (attrs, schema) in [
        (
            json!({"r": "00000000-0000-4000-8000-00000000000"}),
            json!({"r": "uuid"}),
        ),
        (
            json!({"r": "00000000000040008000000000000001"}),
            json!({"r": "uuid"}),
        ),
        (
            json!({"r": "g0000000-0000-4000-8000-000000000001"}),
            json!({"r": "uuid"}),
        ),
        (json!({"r": 7}), json!({"r": "uuid"})),
        (json!({"r": [U1, 7]}), json!({"r": "uuid"})),
        (json!({"b": "AAE"}), json!({"b": "bytes"})),
        (json!({"b": "A-E="}), json!({"b": "bytes"})),
        (json!({"b": big}), json!({"b": "bytes"})),
        (json!({"b": over}), json!({"b": "bytes"})),
        (json!({"b": true}), json!({"b": "bytes"})),
        (
            json!({"b": "AAE="}),
            json!({"b": "bytes", "nobody": "uuid"}),
        ),
        (
            json!({"b": "AAE=", "text": "x"}),
            json!({"b": "bytes", "text": "uuid"}),
        ),
        (json!({"b": "AAE="}), json!({"b": "blob"})),
    ] {
        let body = json!({"durability": "durable", "schema": schema,
            "documents": [{"id": "x", "vector": [1.0, 0.0], "attributes": attrs}]});
        let (s, b) = write(&a, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} was accepted: {b}");
    }
    // Nothing buffered: the index does not exist.
    let (s, b) = query(&a, &json!({"rank_by": ["id", "asc"], "top_k": 10})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "index_not_found", "{b}");

    let body = json!({"durability": "durable", "schema": {"b": "bytes"},
        "documents": [{"id": "x", "vector": [1.0, 0.0], "attributes": {"b": at_most}}]});
    let (s, b) = write(&a, &body).await;
    assert_eq!(s, StatusCode::OK, "4,096 bytes is within the bound: {b}");
}

#[tokio::test]
async fn a_datetime_zone_never_rules_out_a_uuid_or_bytes_row_under_the_same_name() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store);
    // Three writes, one fold: one block, whose `w` holds a datetime, a UUID and bytes.
    for (id, w, ty) in [
        ("d", "2024-01-01T00:00:00Z", "datetime"),
        ("u", U2, "uuid"),
        ("y", "AAE=", "bytes"),
    ] {
        let body = json!({"durability": "durable", "schema": {"w": ty},
            "documents": [{"id": id, "vector": [1.0, 0.0], "attributes": {"w": w}}]});
        let (s, b) = write(&a, &body).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    fold(&a).await;
    for (lit, want) in [("2024-01-01T00:00:00Z", "d"), (U2, "u"), ("AAE=", "y")] {
        let q = json!({"rank_by": ["id", "asc"], "top_k": 10, "filters": ["w", "Eq", lit]});
        let (s, b) = query(&a, &q).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(ids(&b), [want], "{lit}: {b}");
    }
}
