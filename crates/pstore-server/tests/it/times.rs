//! M148: an index's summary reports when it was created and last written, and its bytes.
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
use pstore_server::{Api, FoldPolicy};
use pstore_types::LaneId;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

type A = Arc<Api<MemoryStore>>;

const TENANT: u64 = 148;

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

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// Back from RFC 3339 to Unix milliseconds, refusing anything but three fractional digits.
fn ms(v: &Value) -> u64 {
    let s = v.as_str().unwrap_or_else(|| panic!("not a time: {v}"));
    let frac = s.rsplit_once('.').unwrap().1;
    assert_eq!(frac.len(), 4, "millisecond precision: {s}");
    let micros = pstore_format::datetime::parse(s).unwrap();
    u64::try_from(micros / 1000).unwrap()
}

async fn summary(api: &A) -> Value {
    let (s, b) = send(api, "GET", "/v1/indexes/t", &Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["cost"]["blob_reads"], 1, "one HEAD read: {b}");
    b
}

#[tokio::test]
async fn summary_reports_times_and_bytes() {
    let store = MemoryStore::new();
    let api = Api::unsecured(Accounted::new(store.clone()), LaneId(1)).unwrap();
    let lo = now_ms();
    let (s, b) = send(
        &api,
        "PUT",
        "/v1/indexes/t",
        &json!({"dims": 2, "distance_metric": "dot_product"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let hi = now_ms();
    let b = summary(&api).await;
    let created = ms(&b["created_at"]);
    assert!((lo..=hi).contains(&created), "{lo} <= {created} <= {hi}");
    assert_eq!(b["updated_at"], b["created_at"], "{b}");
    assert_eq!(b["approx_logical_bytes"], 0, "{b}");
    // Two folds, two segments: the bytes are both objects' lengths.
    for id in ["a", "b"] {
        let (s, w) = send(
            &api,
            "PUT",
            "/v1/indexes/t/documents",
            &json!({"durability": "durable", "documents": [{"id": id, "vector": [1.0, 0.5]}]}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{w}");
        fold(&api).await;
    }
    let b = summary(&api).await;
    assert_eq!(b["segments"], 2, "{b}");
    assert_eq!(ms(&b["created_at"]), created, "a write keeps created_at");
    assert!(ms(&b["updated_at"]) >= created, "{b}");
    let prefix = Key::new(format!("{:04x}/tnt/{TENANT}/idx/t/seg/", TENANT as u16));
    let segs = store.list_unrestricted(&prefix).await.unwrap();
    assert_eq!(segs.len(), 2, "{segs:?}");
    let mut sum = 0;
    for k in &segs {
        sum += store.head(k).await.unwrap();
    }
    assert!(sum > 0);
    assert_eq!(b["approx_logical_bytes"], sum, "{b}");
}

#[tokio::test]
async fn an_unfolded_index_reports_no_bytes_and_no_times() {
    let api = Api::unsecured(Accounted::new(MemoryStore::new()), LaneId(1)).unwrap();
    let (s, w) = send(
        &api,
        "PUT",
        "/v1/indexes/t/documents",
        &json!({"documents": [{"id": "a", "vector": [1.0, 0.5]}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{w}");
    let b = summary(&api).await;
    assert_eq!(b["unfolded"], true, "{b}");
    assert_eq!(b["approx_logical_bytes"], 0, "{b}");
    assert_eq!(
        (&b["created_at"], &b["updated_at"]),
        (&Value::Null, &Value::Null),
        "{b}"
    );
}
