//! Source authority is a signed tenant/store pair, checked before storage access.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

mod support;
use axum::http::StatusCode;
use jsonwebtoken::Algorithm;
use pstore_blob::{Accounted, BlobStore, MemoryStore};
use pstore_server::ReplicationPolicy;
use pstore_types::TenantId;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use support::auth::*;

#[tokio::test(start_paused = true)]
async fn signed_sources_permit_real_branch_replication_and_external_copy() {
    let cfg = config(&[("current", &KEY)], "current");
    let (api, store) = api(&cfg);
    api.allow_external_sources(vec!["lake/".to_owned()]);
    let remote = Accounted::new(MemoryStore::new());
    let remote_api =
        pstore_server::Api::with_auth(remote.clone(), pstore_types::LaneId(2), auth(&cfg)).unwrap();
    let far: Arc<dyn BlobStore> = Arc::new(remote.as_tenant(TenantId(0)));
    api.configure_replication(
        ReplicationPolicy {
            scan: Duration::from_secs(10),
            ttl: Duration::from_secs(120),
            max: 64,
            period: Duration::from_secs(1),
            idle: Duration::from_secs(60),
            shards: 4,
        },
        BTreeMap::from([("far".to_owned(), far)]),
    );
    let owner = token("10", &["read", "write", "admin"]);
    let local = token("9", &["read", "write"]);
    for (server, bearer) in [(&api, &local), (&remote_api, &owner)] {
        let got = send(server, "PUT", "/v1/indexes/source/documents", Some(bearer), None,
            &json!({"durability":"durable", "documents":[{"id":"kept", "vector":[1], "attributes":{"n":7}}]})).await;
        assert_eq!(got.status, StatusCode::OK, "{}", got.text);
    }
    let got = send(
        &remote_api,
        "POST",
        "/v1/admin/fold",
        Some(&owner),
        None,
        &json!(null),
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.text);
    let got = send(
        &api,
        "PUT",
        "/v1/indexes/branch/documents",
        Some(&local),
        None,
        &json!({"branch_from_namespace":"source"}),
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.text);
    let mut c = claims("9", &["read", "write"]);
    c["sources"] = json!([{"tenant":"10", "store":"far"}, {"tenant":"10", "store":""}]);
    let grant = sign(&c, Some("current"), Algorithm::HS256, &KEY);
    let got = send(
        &api,
        "PUT",
        "/v1/indexes/replica/replication",
        Some(&grant),
        None,
        &json!({"source":{"index":"source", "tenant":"10", "store":"far"}}),
    )
    .await;
    assert_eq!(got.status, StatusCode::CREATED, "{}", got.text);
    let mut worker = pstore_server::Worker::default();
    for _ in 0..12 {
        api.replication_tick(&mut worker).await;
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    for index in ["branch", "replica"] {
        let got = send(
            &api,
            "POST",
            &format!("/v1/indexes/{index}/query"),
            Some(&local),
            None,
            &json!({"vector":[1], "include_attributes":["n"]}),
        )
        .await;
        assert_eq!(got.status, StatusCode::OK, "{}", got.text);
        assert_eq!(got.body["results"][0]["id"], "kept", "{}", got.text);
        assert_eq!(got.body["results"][0]["attributes"]["n"], 7);
    }
    let raw = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pstore-format/tests/data/good.parquet"
    ))
    .unwrap();
    store
        .as_tenant(TenantId(10))
        .put(&pstore_blob::Key::new("lake/good.parquet"), raw.into())
        .await
        .unwrap();
    let got = send(
        &api,
        "PUT",
        "/v1/indexes/external/external",
        Some(&owner),
        None,
        &json!({"kind":"parquet", "source":"lake/good.parquet", "column":"embedding"}),
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.text);
    let got = send(
        &api,
        "PUT",
        "/v1/indexes/copied/external/copy",
        Some(&grant),
        None,
        &json!({"index":"external", "tenant":"10"}),
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.text);
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pstore-format/tests/data/good.vectors.f32"
    ))
    .unwrap();
    let vector: Vec<f32> = bytes
        .chunks_exact(4)
        .take(8)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let got = send(
        &api,
        "POST",
        "/v1/indexes/copied/external/query",
        Some(&local),
        None,
        &json!({"vector":vector, "k":1}),
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.text);
    assert_eq!(got.body["hits"][0]["row"], 0);
}

#[tokio::test]
async fn source_grants_fence_branch_replication_and_external_copy() {
    let (api, store) = api(&config(&[("current", &KEY)], "current"));
    let remote = Accounted::new(MemoryStore::new());
    let far: Arc<dyn BlobStore> = Arc::new(remote.as_tenant(TenantId(0)));
    api.configure_replication(
        ReplicationPolicy {
            scan: Duration::from_secs(10),
            ttl: Duration::from_secs(120),
            max: 64,
            period: Duration::from_secs(1),
            idle: Duration::from_secs(60),
            shards: 4,
        },
        BTreeMap::from([("far".to_owned(), far)]),
    );
    let write = token("9", &["write"]);
    // Even local same-tenant sources require read authority.
    for field in ["branch_from_namespace", "copy_from_namespace"] {
        let before = requests(&store);
        let got = send(
            &api,
            "PUT",
            "/v1/indexes/destination/documents",
            Some(&write),
            None,
            &json!({field: "source"}),
        )
        .await;
        assert_eq!(got.status, StatusCode::FORBIDDEN, "{}", got.text);
        assert_eq!(got.body["error"]["code"], "forbidden");
        assert_eq!(requests(&store), before);
    }
    // These grants must not authorize their tenant/store cross product.
    let mut c = claims("9", &["write", "read"]);
    c["sources"] = json!([{"tenant":"10", "store":""}, {"tenant":"11", "store":"far"}]);
    let pairs = sign(&c, Some("current"), Algorithm::HS256, &KEY);
    for (tenant, name) in [("10", "far"), ("11", ""), ("12", "far"), ("9", "missing")] {
        for (path, body) in [
            (
                "/v1/indexes/destination/replication",
                json!({"source":{"index":"source", "tenant":tenant, "store":name}}),
            ),
            (
                "/v1/indexes/destination/external/copy",
                json!({"index":"source", "tenant":tenant, "store":name}),
            ),
        ] {
            let before = requests(&store);
            let far_before = requests(&remote);
            let got = send(&api, "PUT", path, Some(&pairs), None, &body).await;
            assert_eq!(
                got.status,
                StatusCode::FORBIDDEN,
                "{tenant}/{name} {path}: {}",
                got.text
            );
            assert_eq!(got.body["error"]["code"], "forbidden");
            assert_eq!(requests(&store), before, "destination touched on refusal");
            assert_eq!(requests(&remote), far_before, "source touched on refusal");
        }
    }
    // Own local source is permitted with both scopes, reaching the missing-source
    // business refusal rather than failing authentication or source authorization.
    let got = send(
        &api,
        "PUT",
        "/v1/indexes/destination/documents",
        Some(&pairs),
        None,
        &json!({"branch_from_namespace":"source"}),
    )
    .await;
    assert_eq!(got.status, StatusCode::BAD_REQUEST, "{}", got.text);
    assert_eq!(got.body["error"]["code"], "bad_request");
    let list = token("9", &["list"]);
    for path in ["/v1/indexes", "/v1/replications"] {
        let got = send(&api, "GET", path, Some(&list), None, &json!(null)).await;
        assert_eq!(got.status, StatusCode::OK, "{}", got.text);
        assert!(
            !got.text.contains("destination"),
            "refusal created destination: {}",
            got.text
        );
    }
}
