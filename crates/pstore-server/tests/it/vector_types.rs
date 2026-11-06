//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "assertions in tests are the reporting mechanism"
)]

//! f16 and i8 vector types (M195): a declared field holds that type's values, and is scored
//! over them exactly.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use http_body_util::BodyExt;
use pstore_blob::{Accounted, MemoryStore};
use pstore_server::Api;
use pstore_testkit::depth::DepthCounting;
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type A<S> = Arc<Api<S>>;

async fn send<S: pstore_blob::BlobStore + 'static>(
    api: &A<S>,
    method: &str,
    uri: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "195")
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

async fn put<S: pstore_blob::BlobStore + 'static>(
    api: &A<S>,
    index: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut body = body;
    body["durability"] = json!("durable");
    send(api, "PUT", &format!("/v1/indexes/{index}/documents"), &body).await
}

async fn write<S: pstore_blob::BlobStore + 'static>(api: &A<S>, index: &str, body: Value) {
    let (s, b) = put(api, index, body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold<S: pstore_blob::BlobStore + 'static>(api: &A<S>) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn query<S: pstore_blob::BlobStore + 'static>(api: &A<S>, index: &str, q: &Value) -> Value {
    let (s, b) = send(api, "POST", &format!("/v1/indexes/{index}/query"), q).await;
    assert_eq!(s, StatusCode::OK, "{q}: {b}");
    b
}

async fn summary<S: pstore_blob::BlobStore + 'static>(api: &A<S>, index: &str) -> Value {
    let (s, b) = send(api, "GET", &format!("/v1/indexes/{index}"), &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

/// The nearest binary16 to `x`, ties to even, `inf` past the largest: computed in f64 from
/// the definition, independently of the format's bit arithmetic.
fn oracle_f16(x: f32) -> f32 {
    let x = f64::from(x);
    if x == 0.0 {
        return x as f32;
    }
    // The spacing of binary16 values at `x`'s magnitude: 2^(e - 10), subnormal below 2^-14.
    let e = x.abs().log2().floor().max(-14.0);
    let step = 2f64.powf(e - 10.0);
    let q = x / step;
    let mut r = q.round();
    if (q - q.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        r -= q.signum();
    }
    let v = r * step;
    if v.abs() > 65504.0 {
        return f32::INFINITY.copysign(x as f32);
    }
    v as f32
}

fn oracle(kind: &str, x: f32) -> f32 {
    match kind {
        "f16" => oracle_f16(x),
        _ => x + 0.0,
    }
}

fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    }
}

/// Edge values every f16 row draws from: ties at 1 and at a subnormal, the largest, and the
/// largest that rounds to it.
const F16_EDGES: [f32; 10] = [
    1.0 + 1.0 / 2048.0,
    1.0 + 3.0 / 2048.0,
    -(1.0 + 1.0 / 2048.0),
    1.5 / 16_777_216.0,
    2.5 / 16_777_216.0,
    0.4 / 16_777_216.0,
    65504.0,
    65519.0,
    -65519.0,
    -0.0,
];

/// Row `i`'s width-16 vector as written: for f16 an edge value or a draw at a scale from 1e-6
/// to 1e4; for i8 integers from -128 to 127 as floats.
fn written(kind: &str, i: usize, next: &mut impl FnMut() -> u64) -> Vec<f32> {
    (0..16)
        .map(|j| {
            let r = next();
            if kind == "i8" {
                return ((r % 256) as i64 - 128) as f32;
            }
            if (i + j).is_multiple_of(5) {
                return F16_EDGES[(i * 7 + j) % F16_EDGES.len()];
            }
            let unit = (r >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0;
            unit * 10f32.powi(((r % 11) as i32) - 6)
        })
        .collect()
}

fn doc(id: &str, v: &[f32]) -> Value {
    json!({"id": id, "vector": v})
}

fn bits(v: &Value) -> Vec<u32> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| (x.as_f64().unwrap() as f32).to_bits())
        .collect()
}

fn hits(b: &Value) -> Vec<(String, u32)> {
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().to_owned(),
                (r["$dist"].as_f64().unwrap() as f32).to_bits(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_declared_field_holds_and_scores_its_types_values() {
    // Criterion 1, both types and every metric.
    for kind in ["f16", "i8"] {
        for metric in ["dot_product", "euclidean_squared", "cosine_distance"] {
            let store = MemoryStore::new();
            let a = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
            let mut next = rng(195);
            let rows: Vec<(String, Vec<f32>)> = (0..450)
                .map(|i| (format!("r{i:03}"), written(kind, i, &mut next)))
                .collect();
            let rounded = |v: &[f32]| v.iter().map(|x| oracle(kind, *x)).collect::<Vec<_>>();
            // `t` is declared; `o` holds the oracle's rounding, undeclared.
            for (from, to) in [(0, 200), (200, 400), (400, 450)] {
                let part = &rows[from..to];
                write(
                    &a,
                    "t",
                    json!({"distance_metric": metric, "schema": {"vector": format!("[16]{kind}")},
                        "documents": part.iter().map(|(id, v)| doc(id, v)).collect::<Vec<_>>()}),
                )
                .await;
                write(
                    &a,
                    "o",
                    json!({"distance_metric": metric, "documents":
                        part.iter().map(|(id, v)| doc(id, &rounded(v))).collect::<Vec<_>>()}),
                )
                .await;
                if to <= 400 {
                    fold(&a).await;
                }
            }
            // Restarted, with the last 50 unfolded in the first process's memory only: read
            // them from it, and the folded ones from a second.
            let b = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
            let all = json!({"rank_by": ["id", "asc"], "top_k": 1000,
                "include_vectors": ["vector"]});
            for (api, n) in [(&a, 450), (&b, 400)] {
                let got = query(api, "t", &all).await;
                let got = got["results"].as_array().unwrap();
                assert_eq!(got.len(), n, "{kind} {metric}");
                if metric == "cosine_distance" {
                    continue;
                }
                for (r, (id, v)) in got.iter().zip(&rows) {
                    assert_eq!(r["id"], id.as_str());
                    let want: Vec<u32> = rounded(v).iter().map(|x| x.to_bits()).collect();
                    assert_eq!(bits(&r["vector"]), want, "{kind} {metric} {id}");
                }
            }
            for q in 0..20 {
                let v = written("f16", 1000 + q, &mut next);
                let q = json!({"vector": v, "top_k": 10, "exact": true});
                for api in [&a, &b] {
                    let (t, o) = (query(api, "t", &q).await, query(api, "o", &q).await);
                    assert_eq!(hits(&t), hits(&o), "{kind} {metric}");
                }
            }
            let meta = summary(&b, "t").await;
            assert_eq!(
                meta["schema"]["vector_types"],
                json!({"vector": format!("[16]{kind}")}),
                "{meta}"
            );
            assert_eq!(meta["schema"]["attributes"], json!({}), "{meta}");
            assert!(
                summary(&b, "o").await["schema"]
                    .get("vector_types")
                    .is_none()
            );
        }
    }
}

#[tokio::test]
async fn named_and_multi_vector_fields_take_a_type() {
    // Criterion 1: a named field and a multi-vector field declared f16.
    let store = MemoryStore::new();
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    let mut next = rng(7);
    let docs: Vec<(String, Vec<f32>, Vec<Vec<f32>>)> = (0..40)
        .map(|i| {
            let title = written("f16", i, &mut next)[..4].to_vec();
            let tokens = (0..1 + i % 3)
                .map(|j| written("f16", i + j, &mut next)[..4].to_vec())
                .collect();
            (format!("d{i:02}"), title, tokens)
        })
        .collect();
    let body = |round: bool| {
        let r = |v: &[f32]| -> Vec<f32> {
            v.iter()
                .map(|x| if round { oracle_f16(*x) } else { *x })
                .collect()
        };
        json!(docs
            .iter()
            .map(|(id, t, ts)| json!({"id": id, "vector": [1.0, 0.0],
                "vectors": {"title": [r(t)], "tokens": ts.iter().map(|v| r(v)).collect::<Vec<_>>()}}))
            .collect::<Vec<_>>())
    };
    write(
        &a,
        "t",
        json!({"schema": {"title": "[4]f16", "tokens": "[4]f16"}, "documents": body(false)}),
    )
    .await;
    write(&a, "o", json!({"documents": body(true)})).await;
    fold(&a).await;
    let got = query(
        &a,
        "t",
        &json!({"rank_by": ["id", "asc"], "top_k": 100, "include_vectors": ["title", "tokens"]}),
    )
    .await;
    for (r, (id, t, ts)) in got["results"].as_array().unwrap().iter().zip(&docs) {
        let want: Vec<u32> = t.iter().map(|x| oracle_f16(*x).to_bits()).collect();
        assert_eq!(bits(&r["vectors"]["title"][0]), want, "{id}");
        for (k, v) in ts.iter().enumerate() {
            let want: Vec<u32> = v.iter().map(|x| oracle_f16(*x).to_bits()).collect();
            assert_eq!(bits(&r["vectors"]["tokens"][k]), want, "{id} token {k}");
        }
    }
    let q = json!({"multi": {"field": "tokens", "vectors": [[0.3, -0.2, 0.9, 0.1]]}, "top_k": 10});
    // A `multi` leg answers by rank, no score: the same ids in the same order.
    assert_eq!(
        ids(&query(&a, "t", &q).await),
        ids(&query(&a, "o", &q).await)
    );
    let q = json!({"vector": [0.3, -0.2, 0.9, 0.1], "field": "title", "top_k": 10, "exact": true});
    assert_eq!(
        hits(&query(&a, "t", &q).await),
        hits(&query(&a, "o", &q).await)
    );
    let meta = summary(&a, "t").await;
    assert_eq!(
        meta["schema"]["vector_types"],
        json!({"title": "[4]f16", "tokens": "[4]f16"}),
        "{meta}"
    );
}

fn ids(b: &Value) -> Vec<String> {
    b["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn f32_base64(v: &[f32]) -> String {
    let raw: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(raw)
}

#[tokio::test]
async fn what_a_type_cannot_hold_is_refused_naming_the_rule() {
    // Criterion 2.
    let store = MemoryStore::new();
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    let decl = |kind: &str, v: Value| json!({"schema": {"vector": format!("[2]{kind}")}, "documents": [{"id": "x", "vector": v}]});
    let cases = [
        (decl("f16", json!([65520.0, 0.0])), "component 0"),
        (decl("f16", json!([0.0, -65520.0])), "component 1"),
        (decl("i8", json!([3.5, 0.0])), "integer from -128 to 127"),
        (decl("i8", json!([0.0, 128.0])), "component 1"),
        (decl("i8", json!([-129.0, 0.0])), "component 0"),
        (
            json!({"schema": {"vector": "[3]f16"}, "documents": [{"id": "x", "vector": [1.0, 2.0]}]}),
            "width 2",
        ),
        (
            json!({"schema": {"n": "[2]f16"}, "documents": [{"id": "x", "vector": [1.0, 2.0],
                "attributes": {"n": 1}}]}),
            "no document carries it as a vector field",
        ),
        (
            json!({"schema": {"sparse": "[2]f16"}, "documents": [{"id": "x", "vector": [1.0, 2.0],
                "sparse": {"k": 1.0}}]}),
            "the sparse field takes no vector type",
        ),
        (
            json!({"schema": {"title": "[2]f16"}, "documents": [{"id": "x", "vector": [1.0, 2.0]}]}),
            "no document carries it",
        ),
        (
            json!({"schema": {"vector": "[2]f16"}, "documents": [{"id": "x", "text": "t"}]}),
            "no document carries it",
        ),
        (
            json!({"schema": {"title": "uuid"}, "documents": [{"id": "x", "vector": [1.0, 2.0],
                "vectors": {"title": [[1.0]]}}]}),
            "is a vector field",
        ),
        (
            json!({"schema": {"n": "f16"}, "documents": [{"id": "x", "vector": [1.0, 2.0],
                "attributes": {"n": "a"}}]}),
            "a write declares datetime",
        ),
        (decl("f16", json!(f32_base64(&[1e9, 0.0]))), "component 0"),
        (decl("i8", json!(f32_base64(&[0.5, 0.0]))), "component 0"),
    ];
    for (body, why) in cases {
        let (s, b) = put(&a, "r", body.clone()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}: {b}");
        let msg = b["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains(why), "{body}: {msg}");
    }
    // Nothing was stored.
    let (s, b) = send(&a, "GET", "/v1/indexes/r", &Value::Null).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");

    // A base64 vector is f32 bytes under every type, read by it.
    write(
        &a,
        "b",
        decl("f16", json!(f32_base64(&[1.0 + 1.0 / 2048.0, 65519.0]))),
    )
    .await;
    write(
        &a,
        "c",
        json!({"schema": {"vector": "[2]i8"}, "upsert_columns": {"id": ["y"], "vector": [[-128.0, 127.0]]}}),
    )
    .await;
    let all = json!({"rank_by": ["id", "asc"], "top_k": 10, "include_vectors": ["vector"]});
    assert_eq!(
        query(&a, "b", &all).await["results"][0]["vector"],
        json!([1.0, 65504.0])
    );
    assert_eq!(
        query(&a, "c", &all).await["results"][0]["vector"],
        json!([-128.0, 127.0])
    );
    // Through columns, a refusal too.
    let (s, b) = put(
        &a,
        "c",
        json!({"upsert_columns": {"id": ["z"], "vector": [[0.5, 1.0]]}}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");

    // `[N]f32` is the default: nothing rounded, nothing recorded.
    write(
        &a,
        "f",
        json!({"schema": {"vector": "[2]f32"}, "documents": [doc("x", &[0.1, 65520.0])]}),
    )
    .await;
    fold(&a).await;
    assert_eq!(
        bits(&query(&a, "f", &all).await["results"][0]["vector"]),
        [0.1_f32.to_bits(), 65520.0_f32.to_bits()]
    );
    assert!(
        summary(&a, "f").await["schema"]
            .get("vector_types")
            .is_none()
    );
    // A named field's type is answered at its width as written, under euclidean too.
    write(
        &a,
        "e",
        json!({"distance_metric": "euclidean_squared", "schema": {"title": "[3]i8"},
            "documents": [{"id": "x", "vector": [1.0, 2.0], "vectors": {"title": [[1.0, 2.0, 3.0]]}}]}),
    )
    .await;
    fold(&a).await;
    let meta = summary(&a, "e").await;
    assert_eq!(
        meta["schema"]["vector_types"],
        json!({"title": "[3]i8"}),
        "{meta}"
    );
}

#[tokio::test]
async fn a_recorded_type_holds_for_every_later_write() {
    // Criterion 3.
    let store = Accounted::new(MemoryStore::new());
    let a = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    // A second process that reads HEAD before the type is recorded.
    let stale = Api::unsecured(store.clone(), LaneId(3)).unwrap();
    write(&stale, "v", json!({"documents": [doc("s0", &[0.5, 0.5])]})).await;
    fold(&stale).await;
    write(
        &a,
        "v",
        json!({"schema": {"vector": "[2]f16"}, "documents": [doc("a", &[1.0 + 1.0 / 2048.0, 2.0])]}),
    )
    .await;
    fold(&a).await;
    // A later vector-only write, undeclared, is rounded: from this process and a fresh one.
    let fresh = Api::unsecured(store.clone(), LaneId(2)).unwrap();
    write(
        &a,
        "v",
        json!({"documents": [doc("b", &[1.0 + 3.0 / 2048.0, 0.0])]}),
    )
    .await;
    write(
        &fresh,
        "v",
        json!({"documents": [doc("c", &[65519.0, 0.0])]}),
    )
    .await;
    let (s, b) = put(
        &fresh,
        "v",
        json!({"documents": [doc("d", &[65520.0, 0.0])]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    // The stale process never read the type: its row is not rounded, and the fold sets it
    // aside.
    write(
        &stale,
        "v",
        json!({"documents": [doc("s1", &[1.0 + 1.0 / 2048.0, 0.0])]}),
    )
    .await;
    // Another type is a conflict naming both.
    let (s, b) = put(
        &fresh,
        "v",
        json!({"schema": {"vector": "[2]i8"}, "documents": [doc("e", &[1.0, 0.0])]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    let msg = b["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("f16") && msg.contains("i8") && msg.contains("`vector`"),
        "{msg}"
    );
    // So is `[N]f32`, recorded or still unfolded: the write would otherwise be rounded.
    let (s, b) = put(
        &fresh,
        "v",
        json!({"schema": {"vector": "[2]f32"}, "documents": [doc("e", &[0.1, 0.0])]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["error"]["code"], "schema_conflict", "{b}");
    let msg = b["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("`vector` as f16") && msg.contains("`vector` as f32"),
        "{msg}"
    );
    write(
        &fresh,
        "u",
        json!({"schema": {"vector": "[2]i8"}, "documents": [doc("u0", &[1.0, 0.0])]}),
    )
    .await;
    let (s, b) = put(
        &fresh,
        "u",
        json!({"schema": {"vector": "[2]f32"}, "documents": [doc("u1", &[0.5, 0.0])]}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`vector` as i8"),
        "{b}"
    );
    fold(&a).await;
    fold(&fresh).await;
    fold(&stale).await;
    let all = json!({"rank_by": ["id", "asc"], "top_k": 10, "include_vectors": ["vector"]});
    let got = query(&fresh, "v", &all).await;
    let rows: BTreeMap<&str, &Value> = got["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].as_str().unwrap(), &r["vector"]))
        .collect();
    assert_eq!(rows["a"], &json!([1.0, 2.0]), "{got}");
    assert_eq!(rows["b"], &json!([1.001_953_1, 0.0]), "{got}");
    assert_eq!(rows["c"], &json!([65504.0, 0.0]), "{got}");
    assert!(!rows.contains_key("s1"), "{got}");
    let meta = summary(&fresh, "v").await;
    assert_eq!(
        meta["schema"]["vector_types"],
        json!({"vector": "[2]f16"}),
        "{meta}"
    );
    assert_eq!(meta["schema"]["attributes"], json!({}), "{meta}");
    assert_eq!(meta["quarantined_rows"], 1, "{meta}");
    let (s, q) = send(&fresh, "GET", "/v1/indexes/v/quarantine", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{q}");
    let reason = q["rows"][0]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("`vector` as f16") && reason.contains("`vector` as f32"),
        "{q}"
    );

    // A type held for an index another process has since dropped is read again before it
    // refuses: the new index of that name takes f32.
    write(
        &a,
        "z",
        json!({"schema": {"vector": "[2]f16"}, "documents": [doc("z0", &[1.0, 0.0])]}),
    )
    .await;
    fold(&a).await;
    assert_eq!(
        summary(&fresh, "z").await["schema"]["vector_types"],
        json!({"vector": "[2]f16"})
    );
    let (s, b) = send(&a, "DELETE", "/v1/indexes/z", &Value::Null).await;
    assert!(s.is_success(), "{b}");
    write(
        &fresh,
        "z",
        json!({"schema": {"vector": "[2]f32"}, "documents": [doc("z1", &[0.1, 0.0])]}),
    )
    .await;

    // Two processes declaring two types before one fold: lane 1's is first in lane order.
    let two = Api::unsecured(store.clone(), LaneId(5)).unwrap();
    write(
        &two,
        "w",
        json!({"schema": {"vector": "[2]i8"}, "documents": [doc("i", &[1.0, 2.0])]}),
    )
    .await;
    write(
        &a,
        "w",
        json!({"schema": {"vector": "[2]f16"}, "documents": [doc("h", &[1.5, 2.0])]}),
    )
    .await;
    fold(&a).await;
    let meta = summary(&a, "w").await;
    assert_eq!(
        meta["schema"]["vector_types"],
        json!({"vector": "[2]f16"}),
        "{meta}"
    );
    assert_eq!(meta["quarantined_rows"], 1, "{meta}");
}

/// A write, a fold and a query's `(depth, requests)`, each, on a fresh store.
async fn costs(schema: Value) -> Vec<(usize, usize)> {
    let store = DepthCounting::new(MemoryStore::new());
    let a = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    let docs: Vec<Value> = (0..20)
        .map(|i| doc(&format!("d{i}"), &[i as f32, 1.0, -2.0, 0.5]))
        .collect();
    let mut out = Vec::new();
    store.reset();
    write(&a, "k", json!({"schema": schema, "documents": docs})).await;
    out.push((store.depth(), store.requests()));
    store.reset();
    fold(&a).await;
    out.push((store.depth(), store.requests()));
    // A fresh process's first vector-only write.
    let b = Api::unsecured(Accounted::new(store.clone()), LaneId(2)).unwrap();
    store.reset();
    write(
        &b,
        "k",
        json!({"documents": [doc("e", &[1.0, 2.0, 3.0, 4.0])]}),
    )
    .await;
    out.push((store.depth(), store.requests()));
    let cold = Api::unsecured(Accounted::new(store.clone()), LaneId(3)).unwrap();
    store.reset();
    query(
        &cold,
        "k",
        &json!({"vector": [1.0, 0.0, 0.0, 0.0], "top_k": 5}),
    )
    .await;
    out.push((store.depth(), store.requests()));
    out
}

#[tokio::test]
async fn a_declared_index_costs_what_an_undeclared_one_does() {
    // Criterion 4.
    let plain = costs(Value::Null).await;
    assert_eq!(costs(json!({"vector": "[4]f16"})).await, plain);
    assert_eq!(costs(json!({"vector": "[4]f32"})).await, plain);
    assert_eq!(plain, PINNED, "measured on the first run");
}

/// A write, a fold, a fresh process's write and a cold query, `(depth, requests)` each.
const PINNED: [(usize, usize); 4] = [(5, 13), (6, 13), (5, 13), (4, 4)];
