//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! uint64 attributes through the API (M172): declared by a write, stored as what they are,
//! answered exactly, and compared with ints and floats by exact value everywhere.

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
    send(api, request("PUT", "/v1/indexes/n/documents", Some(body))).await
}

async fn query(api: &Arc<Api<MemoryStore>>, body: &Value) -> (StatusCode, Value) {
    send(api, request("POST", "/v1/indexes/n/query", Some(body))).await
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

const TWO_53_1: u64 = (1 << 53) + 1;
const I_MAX: u64 = i64::MAX as u64;
const TWO_63: u64 = 1 << 63;

/// What a row's `n` holds, exactly, for the reference.
#[derive(Debug, Clone, Copy)]
enum Exact {
    I(i128),
    F(f64),
}

fn exact(v: &Value) -> Exact {
    v.as_i64()
        .map(|i| Exact::I(i128::from(i)))
        .or_else(|| v.as_u64().map(|u| Exact::I(i128::from(u))))
        .unwrap_or_else(|| Exact::F(v.as_f64().unwrap()))
}

/// The reference order: every float here is integral and within i128, so this is exact.
fn cmp(a: Exact, b: Exact) -> std::cmp::Ordering {
    let whole = |x: Exact| match x {
        Exact::I(i) => i,
        Exact::F(f) => {
            assert!(
                f.fract().abs() < f64::MIN_POSITIVE,
                "the reference only takes integral floats"
            );
            f as i128
        }
    };
    whole(a).cmp(&whole(b))
}

/// `n` by row: declared uint64s a to e, undeclared ints and floats f to i.
fn uints() -> Value {
    json!([
        {"id": "a", "vector": [1.0, 0.0], "attributes": {"n": 0, "ns": [0, u64::MAX]}},
        {"id": "b", "vector": [1.0, 0.0], "attributes": {"n": TWO_53_1}},
        {"id": "c", "vector": [1.0, 0.0], "attributes": {"n": I_MAX}},
        {"id": "d", "vector": [1.0, 0.0], "attributes": {"n": TWO_63, "ns": [TWO_63]}},
        {"id": "e", "vector": [1.0, 0.0], "attributes": {"n": u64::MAX}},
    ])
}

fn others() -> Value {
    json!([
        {"id": "f", "vector": [1.0, 0.0], "attributes": {"n": -1}},
        {"id": "g", "vector": [1.0, 0.0], "attributes": {"n": i64::MAX - 1}},
        {"id": "h", "vector": [1.0, 0.0], "attributes": {"n": 1.9e19}},
        {"id": "i", "vector": [1.0, 0.0], "attributes": {"n": 9.3e18}},
    ])
}

/// The ints and floats first, folded when durable: since M173 a name holds one recorded
/// type, so its other values are rows sealed before the type was recorded.
async fn seeded(store: &Accounted<MemoryStore>, durable: bool) -> Arc<Api<MemoryStore>> {
    let a = api(store);
    for (docs, schema) in [
        (others(), Value::Null),
        (uints(), json!({"n": "uint64", "ns": "uint64"})),
    ] {
        let mut body = json!({"documents": docs});
        if !schema.is_null() {
            body["schema"] = schema;
        }
        if durable {
            body["durability"] = json!("durable");
        }
        let (s, b) = write(&a, &body).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        if durable {
            fold(&a).await;
        }
    }
    a
}

async fn projected(a: &Arc<Api<MemoryStore>>) -> Value {
    let q = json!({"rank_by": ["id", "asc"], "top_k": 10, "include_attributes": true});
    let (s, b) = query(a, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn assert_exact(b: &Value) {
    let r = &b["results"];
    for (i, want) in [0, TWO_53_1, I_MAX, TWO_63, u64::MAX]
        .into_iter()
        .enumerate()
    {
        let got = &r[i]["attributes"]["n"];
        assert_eq!(got.as_u64(), Some(want), "row {i}, an integer exactly: {b}");
    }
    assert_eq!(r[0]["attributes"]["ns"], json!([0, u64::MAX]), "{b}");
    assert_eq!(r[3]["attributes"]["ns"], json!([TWO_63]), "{b}");
    // The body's text, not only its parse: never a float spelling.
    assert!(b.to_string().contains("18446744073709551615"), "{b}");
}

#[tokio::test]
async fn uint64s_answer_exactly_before_and_after_a_fold_and_a_restart() {
    let store = Accounted::new(MemoryStore::new());
    let unfolded = seeded(&store, false).await;
    assert_exact(&projected(&unfolded).await);

    let store = Accounted::new(MemoryStore::new());
    let a = seeded(&store, true).await;
    assert_exact(&projected(&a).await);
    drop(a);
    assert_exact(&projected(&api(&store)).await);
}

#[tokio::test]
async fn filters_compare_uint64s_ints_and_floats_by_exact_value() {
    let store = Accounted::new(MemoryStore::new());
    let a = seeded(&store, true).await;
    let mut rows: Vec<(String, Exact)> = Vec::new();
    for d in uints()
        .as_array()
        .unwrap()
        .iter()
        .chain(others().as_array().unwrap())
    {
        rows.push((
            d["id"].as_str().unwrap().to_owned(),
            exact(&d["attributes"]["n"]),
        ));
    }
    let literals = [
        json!(-1),
        json!(0),
        json!(i64::MAX - 1),
        json!(i64::MAX),
        json!(TWO_63),
        json!(TWO_63 + 1),
        json!(u64::MAX),
        json!(9_223_372_036_854_775_808.0),
        json!(9.3e18),
        json!(1.9e19),
    ];
    for lit in &literals {
        let want_of = |keep: &dyn Fn(std::cmp::Ordering) -> bool| -> Vec<String> {
            rows.iter()
                .filter(|(_, n)| keep(cmp(*n, exact(lit))))
                .map(|(id, _)| id.clone())
                .collect()
        };
        for (op, keep) in [
            (
                "Eq",
                &(|o: std::cmp::Ordering| o.is_eq()) as &dyn Fn(_) -> bool,
            ),
            ("NotEq", &|o: std::cmp::Ordering| o.is_ne()),
            ("Lt", &|o: std::cmp::Ordering| o.is_lt()),
            ("Gte", &|o: std::cmp::Ordering| o.is_ge()),
        ] {
            let mut want = want_of(keep);
            want.sort();
            let q = json!({"rank_by": ["id", "asc"], "top_k": 20, "filters": ["n", op, lit]});
            let (s, b) = query(&a, &q).await;
            assert_eq!(s, StatusCode::OK, "{op} {lit}: {b}");
            assert_eq!(ids(&b), want, "{op} {lit}: {b}");
        }
    }
    let is = |f: Value, want: &'static [&'static str]| {
        let a = Arc::clone(&a);
        async move {
            let q = json!({"rank_by": ["id", "asc"], "top_k": 20, "filters": f});
            let (s, b) = query(&a, &q).await;
            assert_eq!(s, StatusCode::OK, "{f}: {b}");
            assert_eq!(ids(&b), want, "{f}: {b}");
        }
    };
    is(json!(["n", "In", [I_MAX, u64::MAX, -1]]), &["c", "e", "f"]).await;
    is(json!(["n", "In", [9_223_372_036_854_775_808.0]]), &["d"]).await;
    is(json!(["ns", "AnyGt", TWO_63]), &["a"]).await;
    is(json!(["ns", "AnyGt", I_MAX]), &["a", "d"]).await;
    is(json!(["ns", "AnyGt", u64::MAX]), &[]).await;
}

#[tokio::test]
async fn rank_by_facets_and_sums_read_uint64s_as_numbers() {
    let store = Accounted::new(MemoryStore::new());
    let a = seeded(&store, true).await;
    let order = |dir: &str| json!({"rank_by": ["n", dir], "top_k": 20});
    let (s, b) = query(&a, &order("asc")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        ids(&b),
        ["f", "a", "b", "g", "c", "d", "i", "e", "h"],
        "{b}"
    );
    let (s, b) = query(&a, &order("desc")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        ids(&b),
        ["h", "e", "i", "d", "c", "g", "b", "a", "f"],
        "{b}"
    );

    // A sum with a uint64 in it is an integer while it fits u64; one of ints only is not.
    for (ids, want) in [
        (json!(["e"]), json!(u64::MAX)),
        (json!(["e", "f"]), json!(u64::MAX - 1)),
        (json!(["a", "b", "c"]), json!(TWO_53_1 + I_MAX)),
        (json!(["e", "i"]), json!(u64::MAX as f64 + 9.3e18)),
    ] {
        let q = json!({"aggregate_by": {"s": ["Sum", "n"]}, "filters": ["id", "In", ids]});
        let (s, b) = query(&a, &q).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["aggregations"]["s"], want, "{ids}: {b}");
    }
    let ints = json!({"documents": [
        {"id": "x", "vector": [1.0, 0.0], "attributes": {"m": i64::MAX}},
        {"id": "y", "vector": [1.0, 0.0], "attributes": {"m": 1}}]});
    let (s, b) = write(&a, &ints).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let q = json!({"aggregate_by": {"s": ["Sum", "m"]}});
    let (s, b) = query(&a, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(
        b["aggregations"]["s"].is_f64(),
        "ints alone beyond i64 stay a float: {b}"
    );
}

/// `5` declared and `5` not are one group, and so are the float and the uint64 `2^63`,
/// reported as the integer whichever row the group met first.
#[tokio::test]
async fn equal_numbers_are_one_group_and_report_the_integer() {
    for float_first in [true, false] {
        let store = Accounted::new(MemoryStore::new());
        let a = api(&store);
        let float = json!({"documents": [
            {"id": "p", "vector": [1.0, 0.0], "attributes": {"k": 5}},
            {"id": if float_first { "q" } else { "z" }, "vector": [1.0, 0.0],
             "attributes": {"k": 9_223_372_036_854_775_808.0}}]});
        let uint = json!({"schema": {"k": "uint64"}, "documents": [
            {"id": "r", "vector": [1.0, 0.0], "attributes": {"k": 5}},
            {"id": "s", "vector": [1.0, 0.0], "attributes": {"k": TWO_63}}]});
        // Each folded: the untyped rows are sealed before `k`'s type is recorded (M173).
        for body in [&float, &uint] {
            let mut body = body.clone();
            body["durability"] = json!("durable");
            let (s, b) = write(&a, &body).await;
            assert_eq!(s, StatusCode::OK, "{b}");
            fold(&a).await;
        }
        let q = json!({"aggregate_by": {"n": ["Count", "id"]}, "group_by": ["k"]});
        let (s, b) = query(&a, &q).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            b["aggregation_groups"],
            json!([{"k": 5, "n": 2}, {"k": TWO_63, "n": 2}]),
            "float first: {float_first}: {b}"
        );
    }
}

#[tokio::test]
async fn a_boost_and_a_ref_new_condition_read_uint64s_as_filters_do() {
    let store = Accounted::new(MemoryStore::new());
    let a = seeded(&store, true).await;
    let t = json!({"attr": "n", "fn": "eq", "weight": 100.0, "value": u64::MAX});
    let q = json!({"vector": [1.0, 0.0], "top_k": 9, "boost": {"terms": [t]}});
    let (s, b) = query(&a, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ids(&b)[0], "e", "the boosted row first: {b}");
    // A numeric function reads a uint64 as the f64 it scores in: 1.9e19, then u64::MAX,
    // then 9.3e18; below it 2^63 and i64::MAX tie as f64s.
    let t = json!({"attr": "n", "fn": "linear", "weight": 1e-9});
    let q = json!({"vector": [1.0, 0.0], "top_k": 3, "boost": {"terms": [t]}});
    let (s, b) = query(&a, &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ids(&b), ["h", "e", "i"], "{b}");

    // Replaces `e` only when the stored uint64 equals the new one.
    for (n, applies) in [(u64::MAX - 1, false), (u64::MAX, true)] {
        let body = json!({"durability": "durable", "schema": {"n": "uint64"},
            "documents": [{"id": "e", "vector": [1.0, 0.0], "attributes": {"n": n, "m": 1}}],
            "upsert_condition": ["n", "Eq", {"$ref_new": "n"}]});
        let (s, b) = write(&a, &body).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        fold(&a).await;
        let only_e = json!({"rank_by": ["id", "asc"], "top_k": 1,
                            "filters": ["id", "Eq", "e"], "include_attributes": ["m"]});
        let (s, b) = query(&a, &only_e).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let m = &b["results"][0]["attributes"]["m"];
        assert_eq!(m == &json!(1), applies, "{n}: {b}");
    }
}

#[tokio::test]
async fn malformed_uint64s_are_refused_and_nothing_is_buffered() {
    let store = Accounted::new(MemoryStore::new());
    let a = api(&store);
    let declared = json!({"n": "uint64"});
    for (attrs, schema, says) in [
        (json!({"n": -1}), declared.clone(), "uint64"),
        (json!({"n": 5.0}), declared.clone(), "uint64"),
        (json!({"n": 1e3}), declared.clone(), "uint64"),
        (json!({"n": "5"}), declared.clone(), "uint64"),
        (json!({"n": true}), declared.clone(), "uint64"),
        (json!({"n": [1, -1]}), declared.clone(), "uint64"),
        (json!({"n": TWO_63}), Value::Null, "uint64"),
        (
            json!({"n": 1}),
            json!({"n": "uint64", "nobody": "uint64"}),
            "nobody",
        ),
    ] {
        let mut body = json!({"durability": "durable",
            "documents": [{"id": "x", "vector": [1.0, 0.0], "attributes": attrs}]});
        if !schema.is_null() {
            body["schema"] = schema;
        }
        let (s, b) = write(&a, &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body} was accepted: {b}");
        let message = b["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains(says), "{body}: {b}");
    }
    let (s, b) = query(&a, &json!({"rank_by": ["id", "asc"], "top_k": 10})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "index_not_found", "{b}");
}
