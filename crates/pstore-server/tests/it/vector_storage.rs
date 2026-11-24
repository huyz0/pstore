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

//! M289: an index's dense vectors stored f16 or i8, by `vector_storage`: the cluster's
//! default (`PSTORE_VECTOR_STORAGE`) or the index's override. Lossless where a declared type
//! (M195) fits the storage under dot product and euclidean; otherwise a folded row answers
//! and scores the codec's values.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_format::BlockOptions;
use pstore_format::narrow::{self, Elem, Vectors};
use pstore_server::{Api, Config};
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const TENANT: u64 = 289;

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", TENANT.to_string())
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    let res = Arc::clone(api).router().oneshot(r).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn write(api: &A, index: &str, body: Value) {
    let mut body = body;
    body["durability"] = json!("durable");
    let (s, b) = send(api, "PUT", &format!("/v1/indexes/{index}/documents"), &body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn fold(api: &A) {
    let (s, b) = send(api, "POST", "/v1/admin/fold", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn query(api: &A, index: &str, q: &Value) -> Value {
    let (s, b) = send(api, "POST", &format!("/v1/indexes/{index}/query"), q).await;
    assert_eq!(s, StatusCode::OK, "{q}: {b}");
    b
}

async fn patch(api: &A, index: &str, storage: Value) -> (StatusCode, Value) {
    send(
        api,
        "PATCH",
        &format!("/v1/indexes/{index}"),
        &json!({ "storage": storage }),
    )
    .await
}

async fn storage(api: &A, index: &str) -> Value {
    let (s, b) = send(api, "GET", &format!("/v1/indexes/{index}"), &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["storage"].clone()
}

/// Each of the index's segments' footer versions, in key order.
async fn versions(store: &Accounted<MemoryStore>, index: &str) -> Vec<u16> {
    let prefix = Key::new(format!(
        "{:04x}/tnt/{TENANT}/idx/{index}/seg/",
        TENANT as u16
    ));
    let store = store.as_tenant(pstore_types::TenantId(u128::from(TENANT)));
    let mut out = Vec::new();
    for k in store.list_unrestricted(&prefix).await.unwrap() {
        if !k.as_str().ends_with(".seg") {
            continue;
        }
        let b = store.get(&k).await.unwrap();
        let at = b.len() - (8 + 2 + 8 + 4 + 4 + 8 + 8) + 8;
        out.push(u16::from_le_bytes([b[at], b[at + 1]]));
    }
    out
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

/// Width-16 components a `kind` field holds exactly: integers from -128 to 127 for i8; for
/// f16 draws at scales from 1e-4 to 1e3, which the door rounds to f16.
fn written(kind: &str, next: &mut impl FnMut() -> u64) -> Vec<f32> {
    (0..16)
        .map(|_| {
            let r = next();
            if kind == "i8" {
                return ((r % 256) as i64 - 128) as f32;
            }
            let unit = (r >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0;
            unit * 10f32.powi(((r % 8) as i32) - 4)
        })
        .collect()
}

fn bits(v: &Value) -> Vec<u32> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| (x.as_f64().unwrap() as f32).to_bits())
        .collect()
}

fn floats(v: &Value) -> Vec<f32> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap() as f32)
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

/// What the codec makes of an answered vector under `elem` (no tail: an answer has none).
fn coded(elem: Elem, v: &[f32]) -> Vec<f32> {
    let mut raw = Vec::new();
    narrow::encode(elem, v, v.len(), 0, &mut raw);
    narrow::decode(elem, &raw, v.len(), 0).unwrap()
}

const ALL: &str = r#"{"rank_by": ["id", "asc"], "top_k": 1000, "include_vectors": ["vector"]}"#;

/// Writes the same rows to `n` and `f`, in two folds: the first f32 for both, the second
/// `n`'s by its override to `storage` (an index is patched once it exists).
async fn twins(api: &A, kind: Option<&str>, metric: &str, storage: &str, rows: &[Vec<f32>]) {
    for (from, to) in [(0, 120), (120, 240)] {
        for index in ["n", "f"] {
            let mut body = json!({"distance_metric": metric, "documents": (from..to)
                .map(|i| json!({"id": format!("r{i:03}"), "vector": rows[i]}))
                .collect::<Vec<_>>()});
            if let Some(k) = kind {
                body["schema"] = json!({"vector": format!("[16]{k}")});
            }
            write(api, index, body).await;
        }
        fold(api).await;
        if from == 0 {
            let (s, b) = patch(api, "n", json!({"vector_storage": storage})).await;
            assert_eq!(s, StatusCode::OK, "{b}");
        }
    }
}

#[tokio::test]
async fn a_declared_type_its_storage_holds_answers_bit_equal_to_f32() {
    // Criterion 3: lossless under dot product and euclidean.
    for (kind, storage) in [("f16", "f16"), ("i8", "i8"), ("i8", "f16")] {
        for metric in ["dot_product", "euclidean_squared"] {
            let store = Accounted::new(MemoryStore::new());
            let a = Api::unsecured(store.clone(), LaneId(1)).unwrap();
            let mut next = rng(289);
            let rows: Vec<Vec<f32>> = (0..240).map(|_| written(kind, &mut next)).collect();
            twins(&a, Some(kind), metric, storage, &rows).await;
            assert_eq!(
                versions(&store, "n").await,
                [3, 4],
                "{kind} {storage} {metric}"
            );
            assert_eq!(versions(&store, "f").await, [3, 3]);
            // Read by a second process: every answer from the segments.
            let b = Api::unsecured(store.clone(), LaneId(2)).unwrap();
            let all: Value = serde_json::from_str(ALL).unwrap();
            let (n, f) = (query(&b, "n", &all).await, query(&b, "f", &all).await);
            let (n, f) = (
                n["results"].as_array().unwrap(),
                f["results"].as_array().unwrap(),
            );
            assert_eq!(n.len(), 240);
            for (x, y) in n.iter().zip(f) {
                assert_eq!(x["id"], y["id"]);
                assert_eq!(bits(&x["vector"]), bits(&y["vector"]), "{kind} {metric}");
            }
            for _ in 0..20 {
                let q = json!({"vector": written("f16", &mut next), "top_k": 10, "exact": true});
                assert_eq!(
                    hits(&query(&b, "n", &q).await),
                    hits(&query(&b, "f", &q).await),
                    "{kind} {storage} {metric}"
                );
            }
        }
    }
}

#[tokio::test]
async fn an_undeclared_or_cosine_field_answers_the_codecs_values_once_folded() {
    // Criterion 3's other half, and 5: f32 answers what was written, a narrow mode the
    // codec's rounding of it -- once folded; unfolded rows answer from the log, unrounded.
    for storage in [Elem::F16, Elem::I8] {
        for (kind, metric) in [
            (None, "dot_product"),
            (None, "euclidean_squared"),
            (Some("f16"), "cosine_distance"),
        ] {
            let store = Accounted::new(MemoryStore::new());
            let a = Api::unsecured(store.clone(), LaneId(1)).unwrap();
            let mut next = rng(17);
            let rows: Vec<Vec<f32>> = (0..240).map(|_| written("f16", &mut next)).collect();
            twins(&a, kind, metric, storage.name(), &rows).await;
            // Unfolded rows in the first process: as f32 answers them.
            let late: Vec<Value> = (240..250)
                .map(|i| json!({"id": format!("r{i:03}"), "vector": rows[i - 240]}))
                .collect();
            for index in ["n", "f"] {
                write(
                    &a,
                    index,
                    json!({"distance_metric": metric, "documents": late}),
                )
                .await;
            }
            let all: Value = serde_json::from_str(ALL).unwrap();
            let (n, f) = (query(&a, "n", &all).await, query(&a, "f", &all).await);
            let (n, f) = (
                n["results"].as_array().unwrap(),
                f["results"].as_array().unwrap(),
            );
            assert_eq!(n.len(), 250);
            let mut changed = 0;
            for (i, (x, y)) in n.iter().zip(f).enumerate() {
                // The second fold's rows are narrow; the first's f32, the last unfolded.
                let narrow = (120..240).contains(&i);
                let want = if narrow {
                    coded(storage, &floats(&y["vector"]))
                } else {
                    floats(&y["vector"])
                };
                let got = floats(&x["vector"]);
                if narrow {
                    changed += usize::from(got != floats(&y["vector"]));
                }
                let (got, want): (Vec<u32>, Vec<u32>) = (
                    got.iter().map(|v| v.to_bits()).collect(),
                    want.iter().map(|v| v.to_bits()).collect(),
                );
                assert_eq!(got, want, "{storage:?} {metric} row {i}");
            }
            // Lossy here: most folded rows differ from f32's answer.
            assert!(
                changed > 60,
                "{storage:?} {metric}: {changed} of 120 changed"
            );
        }
    }
}

#[tokio::test]
async fn the_setting_takes_the_cluster_default_and_an_index_override() {
    // Criterion 4.
    let store = Accounted::new(MemoryStore::new());
    let api = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    api.set_block_options(BlockOptions {
        vectors: Vectors {
            elem: Elem::F16,
            tail: 0,
        },
        ..BlockOptions::default()
    });
    let doc = |i: usize| json!({"id": format!("d{i}"), "vector": [1.0, 0.5, f32::from(i as u8)]});
    let put = |from: usize| json!({"documents": (from..from + 10).map(doc).collect::<Vec<_>>()});
    write(&api, "t", put(0)).await;
    fold(&api).await;
    let s = storage(&api, "t").await;
    assert_eq!(s["vector_storage"], "f16", "{s}");
    assert_eq!(s["overrides"], json!({}));
    assert_eq!(versions(&store, "t").await, [4]);
    // An override wins, keeps M231's settings, and survives into a second server.
    let (st, b) = patch(&api, "t", json!({"block_aggregates": false})).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let (st, b) = patch(&api, "t", json!({"vector_storage": "f32"})).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        b["storage"]["overrides"],
        json!({"block_aggregates": false, "vector_storage": "f32"})
    );
    let other = Api::unsecured(store.clone(), LaneId(2)).unwrap();
    other.set_block_options(BlockOptions {
        vectors: Vectors {
            elem: Elem::I8,
            tail: 0,
        },
        ..BlockOptions::default()
    });
    write(&other, "t", put(10)).await;
    fold(&other).await;
    assert_eq!(versions(&store, "t").await, [4, 3]);
    let s = storage(&other, "t").await;
    assert_eq!(s["vector_storage"], "f32", "{s}");
    // Cleared, the sealing server's default returns.
    let (st, b) = patch(&other, "t", json!({"vector_storage": null})).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        b["storage"]["overrides"],
        json!({"block_aggregates": false})
    );
    assert_eq!(b["storage"]["vector_storage"], "i8");
    write(&other, "t", put(20)).await;
    fold(&other).await;
    assert_eq!(versions(&store, "t").await, [4, 3, 4]);
    // Under row blocks nothing narrows, and the summary says so.
    let (st, b) = patch(&other, "t", json!({"block_layout": "rows"})).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["storage"]["vector_storage"], "f32", "{b}");
    let (st, b) = patch(&other, "t", json!({"block_layout": null})).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["storage"]["vector_storage"], "i8", "{b}");
    // Every row reads back, from every segment.
    let all = query(&api, "t", &json!({"rank_by": ["id", "asc"], "top_k": 100})).await;
    assert_eq!(all["results"].as_array().unwrap().len(), 30);
    // Refused: an unknown name by name, a value of another type as malformed.
    let (st, b) = patch(&api, "t", json!({"vector_storage": "bf16"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b.to_string().contains(
            r#"storage.vector_storage is \"f32\", \"f16\", \"i8\" or null, not \"bf16\""#
        ),
        "{b}"
    );
    for bad in [json!(16), json!(true)] {
        let (st, b) = patch(&api, "t", json!({ "vector_storage": bad })).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}: {b}");
    }
    assert_eq!(storage(&other, "t").await["vector_storage"], "i8");
}

#[test]
fn the_cluster_default_is_read_from_the_environment() {
    let vars = |v: Option<&str>| {
        let mut m = BTreeMap::from([("PSTORE_LANE".to_owned(), "1".to_owned())]);
        if let Some(v) = v {
            m.insert("PSTORE_VECTOR_STORAGE".to_owned(), v.to_owned());
        }
        move |k: &str| m.get(k).cloned()
    };
    for (v, want) in [
        (None, Elem::F32),
        (Some("f32"), Elem::F32),
        (Some("f16"), Elem::F16),
        (Some("i8"), Elem::I8),
    ] {
        let c = Config::from_vars(vars(v)).unwrap();
        assert_eq!(
            c.blocks.vectors,
            Vectors {
                elem: want,
                tail: 0
            },
            "{v:?}"
        );
    }
    for bad in ["F16", "int8", ""] {
        let e = Config::from_vars(vars(Some(bad))).unwrap_err();
        assert!(
            e.to_string()
                .contains(&format!("PSTORE_VECTOR_STORAGE={bad}")),
            "{e}"
        );
    }
}
