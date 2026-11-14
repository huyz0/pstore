//! M231: an index's storage settings. The cluster's default comes from `PSTORE_BLOCK_LAYOUT`,
//! `PSTORE_EQUALITY_FILTERS` and `PSTORE_BLOCK_AGGREGATES`; `PATCH /v1/indexes/{index}`
//! overrides it per index, and the index's summary says what its next fold writes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_format::BlockOptions;
use pstore_server::{Api, Config, FoldPolicy};
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const TENANT: u64 = 231;

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |k| map.get(k).cloned()
}

#[test]
fn the_cluster_default_is_read_from_the_environment() {
    let c = Config::from_vars(vars(&[("PSTORE_LANE", "1")])).unwrap();
    assert_eq!(c.blocks, BlockOptions::default());
    let c = Config::from_vars(vars(&[
        ("PSTORE_LANE", "1"),
        ("PSTORE_BLOCK_LAYOUT", "rows"),
        ("PSTORE_EQUALITY_FILTERS", "off"),
        ("PSTORE_BLOCK_AGGREGATES", "on"),
    ]))
    .unwrap();
    assert_eq!(
        c.blocks,
        BlockOptions {
            columnar: false,
            equality: false,
            aggregates: true,
        }
    );
    let c = Config::from_vars(vars(&[
        ("PSTORE_LANE", "1"),
        ("PSTORE_BLOCK_LAYOUT", "columnar"),
        ("PSTORE_BLOCK_AGGREGATES", "off"),
    ]))
    .unwrap();
    assert_eq!(
        c.blocks,
        BlockOptions {
            columnar: true,
            equality: true,
            aggregates: false,
        }
    );
    for (var, bad) in [
        ("PSTORE_BLOCK_LAYOUT", "row"),
        ("PSTORE_EQUALITY_FILTERS", "true"),
        ("PSTORE_BLOCK_AGGREGATES", ""),
    ] {
        let e = Config::from_vars(vars(&[("PSTORE_LANE", "1"), (var, bad)])).unwrap_err();
        assert!(e.to_string().contains(&format!("{var}={bad}")), "{e}");
    }
}

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
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
    let res = Arc::clone(api).router().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn fold(api: &A) {
    api.fold_due(&FoldPolicy {
        period: std::time::Duration::from_secs(1),
        age: std::time::Duration::ZERO,
        bytes: 1,
    })
    .await;
}

async fn write(api: &A, from: usize, to: usize) {
    let docs: Vec<Value> = (from..to)
        .map(|i| json!({"id": format!("d{i}"), "vector": [1.0, 0.5], "attributes": {"n": i}}))
        .collect();
    let (s, b) = send(
        api,
        "PUT",
        "/v1/indexes/t/documents",
        &json!({"durability": "durable", "documents": docs}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    fold(api).await;
}

async fn storage(api: &A) -> Value {
    let (s, b) = send(api, "GET", "/v1/indexes/t", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["storage"].clone()
}

/// Whether each of the index's segments, in key order, is columnar: the footer's version.
async fn columnar(store: &Accounted<MemoryStore>) -> Vec<bool> {
    let prefix = Key::new(format!("{:04x}/tnt/{TENANT}/idx/t/seg/", TENANT as u16));
    let store = store.as_tenant(pstore_types::TenantId(u128::from(TENANT)));
    let mut out = Vec::new();
    for k in store.list_unrestricted(&prefix).await.unwrap() {
        if !k.as_str().ends_with(".seg") {
            continue;
        }
        let b = store.get(&k).await.unwrap();
        let at = b.len() - (8 + 2 + 8 + 4 + 4 + 8 + 8) + 8;
        out.push(u16::from_le_bytes([b[at], b[at + 1]]) == 3);
    }
    out
}

#[tokio::test]
async fn an_index_overrides_the_cluster_default_and_says_so() {
    let store = Accounted::new(MemoryStore::new());
    let api = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    // The cluster default is rows, as an operator mid-rollout sets it.
    api.set_block_options(BlockOptions {
        columnar: false,
        ..BlockOptions::default()
    });
    // Absent: nothing to override.
    let patch = json!({"storage": {"block_layout": "columnar"}});
    let (s, b) = send(&api, "PATCH", "/v1/indexes/t", &patch).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    write(&api, 0, 20).await;
    assert_eq!(
        storage(&api).await,
        json!({"block_layout": "rows", "equality_filters": true, "block_aggregates": true,
               "overrides": {}})
    );
    assert_eq!(columnar(&store).await, [false]);

    let (s, b) = send(&api, "PATCH", "/v1/indexes/t", &patch).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let want = json!({"block_layout": "columnar", "equality_filters": true,
                      "block_aggregates": true, "overrides": {"block_layout": "columnar"}});
    assert_eq!(b["storage"], want);
    assert!(b.get("read_only").is_none(), "{b}");
    assert_eq!(storage(&api).await, want);
    write(&api, 20, 40).await;
    assert_eq!(columnar(&store).await, [false, true]);

    // One setting changed keeps the other; `null` clears an override.
    let (s, b) = send(
        &api,
        "PATCH",
        "/v1/indexes/t",
        &json!({"storage": {"block_aggregates": false}, "read_only": false}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["read_only"], false);
    assert_eq!(
        b["storage"]["overrides"],
        json!({"block_layout": "columnar", "block_aggregates": false})
    );
    let (s, b) = send(
        &api,
        "PATCH",
        "/v1/indexes/t",
        &json!({"storage": {"block_layout": null, "block_aggregates": null}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["storage"]["overrides"], json!({}));
    assert_eq!(b["storage"]["block_layout"], "rows");
    write(&api, 40, 60).await;
    assert_eq!(columnar(&store).await, [false, true, false]);

    // Refused by name: an unknown setting, a value it does not take, an empty patch.
    for (body, says) in [
        (
            json!({"storage": {"layout": "rows"}}),
            "unknown field `layout`",
        ),
        (json!({"storage": {"block_layout": "row"}}), "block_layout"),
        (
            json!({"storage": {"equality_filters": "off"}}),
            "malformed index patch",
        ),
        (
            json!({"storage": {"block_layout": 1}}),
            "malformed index patch",
        ),
        (json!({}), "names read_only, storage"),
    ] {
        let (s, b) = send(&api, "PATCH", "/v1/indexes/t", &body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}: {b}");
        assert!(b.to_string().contains(says), "{body}: {b}");
    }
}
