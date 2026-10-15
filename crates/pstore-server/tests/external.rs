//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M86: external indexes over the API. Off unless the deployment allows reading a customer's
//! objects, and then only under the prefixes it names.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pstore_blob::{Accounted, BlobStore, Key, MemoryStore};
use pstore_server::{Api, Config, ConfigError};
use pstore_types::{LaneId, TenantId};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../pstore-format/tests/data/");
type A = Arc<Api<MemoryStore>>;

/// A server whose store holds the customer's file, with the sources off unless `prefixes`.
async fn api(prefixes: Option<&[&str]>) -> (A, Accounted<MemoryStore>) {
    let store = Accounted::new(MemoryStore::new());
    let api = Api::new(store.clone(), LaneId(1)).unwrap();
    if let Some(p) = prefixes {
        api.allow_external_sources(p.iter().map(|s| (*s).to_owned()).collect());
    }
    let raw = std::fs::read(format!("{DATA}good.parquet")).unwrap();
    store
        .as_tenant(TenantId(86))
        .put(&Key::new("lake/good.parquet"), raw.into())
        .await
        .unwrap();
    (api, store)
}

async fn send(api: &A, method: &str, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-pstore-tenant", "86")
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

fn register() -> Value {
    json!({"kind": "parquet", "source": "lake/good.parquet", "column": "embedding"})
}

fn row0() -> Vec<f32> {
    std::fs::read(format!("{DATA}good.vectors.f32"))
        .unwrap()
        .chunks_exact(4)
        .take(8)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect()
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn external_sources_are_off_by_default() {
    let (api, _) = api(None).await;
    let (s, b) = send(&api, "PUT", "/v1/indexes/x/external", &register()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "external_sources_off"),
        "{b}"
    );
    let q = json!({"vector": row0(), "k": 3});
    let (s, b) = send(&api, "POST", "/v1/indexes/x/external/query", &q).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "external_sources_off"),
        "{b}"
    );
}

#[tokio::test]
async fn an_allowed_source_is_registered_queried_and_dropped_by_name() {
    let (api, _) = api(Some(&["lake/"])).await;
    let (s, b) = send(&api, "PUT", "/v1/indexes/x/external", &register()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["epoch"].as_u64().unwrap() > 0);
    assert!(b["cost"]["blob_reads"].as_u64().unwrap() > 0);

    let q = json!({"vector": row0(), "k": 3});
    let (s, b) = send(&api, "POST", "/v1/indexes/x/external/query", &q).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let hits = b["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0]["row"], 0, "{b}");
    assert!(hits[0]["score"].as_f64().unwrap() > 0.999);
    assert!(b["meta"]["cost"]["blob_reads"].as_u64().unwrap() > 0);

    let (s, b) = send(&api, "DELETE", "/v1/indexes/x/external", &json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(&api, "POST", "/v1/indexes/x/external/query", &q).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "external_index_not_found"),
        "{b}"
    );
    let (s, b) = send(&api, "DELETE", "/v1/indexes/x/external", &json!({})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "external_index_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn each_refusal_has_its_status_and_code() {
    let (api, store) = api(Some(&["lake/"])).await;
    let put = |body: Value| {
        let api = Arc::clone(&api);
        async move { send(&api, "PUT", "/v1/indexes/x/external", &body).await }
    };
    // Outside the prefixes, and trying to walk out of them.
    for source in ["other/good.parquet", "lake/../secret"] {
        let (s, b) = put(json!({"kind": "parquet", "source": source, "column": "embedding"})).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::FORBIDDEN, "external_source_not_allowed"),
            "{b}"
        );
    }
    // A column the reader cannot serve, with the reader's reason; a source with no object.
    let (s, b) =
        put(json!({"kind": "parquet", "source": "lake/good.parquet", "column": "nope"})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::BAD_REQUEST, "external_source_unreadable"),
        "{b}"
    );
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no column"),
        "{b}"
    );
    let (s, b) =
        put(json!({"kind": "parquet", "source": "lake/gone.parquet", "column": "embedding"})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::BAD_REQUEST, "external_source_unreadable"),
        "{b}"
    );
    // A body that is not a registration.
    for body in [
        json!({"kind": "csv", "source": "lake/good.parquet", "column": "embedding"}),
        json!({"kind": "parquet", "source": "lake/good.parquet"}),
        json!({"source": "lake/good.parquet", "column": "embedding"}),
    ] {
        let (s, b) = put(body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    }
    // A name taken.
    assert_eq!(put(register()).await.0, StatusCode::OK);
    let (s, b) = put(register()).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "index_exists"), "{b}");
    // A query the library refuses, and bounds on how much it may read.
    let q = |v: Value| {
        let api = Arc::clone(&api);
        async move { send(&api, "POST", "/v1/indexes/x/external/query", &v).await }
    };
    let (s, b) = q(json!({"vector": [1.0, 0.0], "k": 3})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::BAD_REQUEST, "external_source_unreadable"),
        "{b}"
    );
    for body in [
        json!({"vector": row0(), "k": 0}),
        json!({"vector": row0(), "k": 1001}),
        json!({"vector": row0(), "k": 5, "window": 10_001}),
        json!({"vector": row0(), "k": 5, "window": 2}),
        json!({"k": 5}),
    ] {
        let (s, b) = q(body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    }
    // The customer rewrites the file: an error, never a stale answer.
    let mut raw = std::fs::read(format!("{DATA}good.parquet")).unwrap();
    raw[4] ^= 0xFF;
    store
        .as_tenant(TenantId(86))
        .put(&Key::new("lake/good.parquet"), raw.into())
        .await
        .unwrap();
    let (s, b) = q(json!({"vector": row0(), "k": 3})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "external_source_changed"),
        "{b}"
    );
}

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |k| map.get(k).cloned()
}

#[test]
fn the_deployment_opts_in_by_name_and_names_its_prefixes() {
    let lane = ("PSTORE_LANE", "1");
    assert_eq!(
        Config::from_vars(vars(&[lane])).unwrap().external_sources,
        None
    );
    let c = Config::from_vars(vars(&[
        lane,
        ("PSTORE_EXTERNAL_SOURCES", "allow"),
        ("PSTORE_EXTERNAL_PREFIXES", "lake/, warehouse/exports/ ,"),
    ]))
    .unwrap();
    assert_eq!(
        c.external_sources,
        Some(vec!["lake/".to_owned(), "warehouse/exports/".to_owned()])
    );
    // `off` is explicit and means none, prefixes or not.
    let c = Config::from_vars(vars(&[
        lane,
        ("PSTORE_EXTERNAL_SOURCES", "off"),
        ("PSTORE_EXTERNAL_PREFIXES", "lake/"),
    ]))
    .unwrap();
    assert_eq!(c.external_sources, None);
    // Anything else, and `allow` with nothing to allow, is refused by name.
    let e = Config::from_vars(vars(&[lane, ("PSTORE_EXTERNAL_SOURCES", "yes")]));
    assert_eq!(e, Err(ConfigError::ExternalSources("yes".to_owned())));
    let e = Config::from_vars(vars(&[lane, ("PSTORE_EXTERNAL_SOURCES", "allow")]));
    assert_eq!(e, Err(ConfigError::ExternalPrefixes));
    let e = Config::from_vars(vars(&[
        lane,
        ("PSTORE_EXTERNAL_SOURCES", "allow"),
        ("PSTORE_EXTERNAL_PREFIXES", " , "),
    ]));
    assert_eq!(e, Err(ConfigError::ExternalPrefixes));
}
