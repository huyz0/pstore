//! A public Api must require an explicit authentication policy before serving data.
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
use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_server::Api;
use pstore_types::LaneId;
use serde_json::Value;
use tower::ServiceExt;

#[test]
fn peering_without_tls_is_refused_before_blob_io() {
    let store = Accounted::new(MemoryStore::new());
    let api = Api::unsecured(store.clone(), LaneId(1)).unwrap();
    let result = api.set_peers(Some(pstore_server::PeerConfig {
        servers: vec![
            "https://127.0.0.1:1".to_owned(),
            "https://127.0.0.1:2".to_owned(),
        ],
        me: 0,
        timeout: std::time::Duration::from_secs(1),
    }));
    let error = result.expect_err("even private public APIs need mTLS for peers");
    assert!(error.to_string().contains("TLS"), "{error}");
    assert!(api.peer_list().is_empty());
    for class in [
        OpClass::Read,
        OpClass::Write,
        OpClass::List,
        OpClass::Delete,
    ] {
        assert_eq!(store.total(class), 0);
    }
}

#[test]
fn standalone_peer_configuration_is_validated_before_store_access_or_binding() {
    for (extra, expected) in [
        (None, "PSTORE_PEER_BIND"),
        (Some(("PSTORE_PEER_BIND", "127.0.0.1:9443")), "TLS"),
    ] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pstore-server"));
        command
            .env_clear()
            .env("PSTORE_ALLOW_UNAUTHENTICATED", "1")
            .env("PSTORE_LANE", "1")
            .env("PSTORE_BACKEND", "s3")
            .env("PSTORE_S3_ENDPOINT", "not-a-url")
            .env("PSTORE_BUCKET", "test")
            .env(
                "PSTORE_PEERS",
                "https://127.0.0.1:9443,https://127.0.0.1:9444",
            )
            .env("PSTORE_PEER_SELF", "https://127.0.0.1:9443");
        if let Some((key, value)) = extra {
            command.env(key, value);
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(stderr.contains(expected), "{stderr}");
        assert!(
            !stderr.contains("cannot open"),
            "peer configuration must precede backend construction: {stderr}"
        );
    }
}

#[test]
fn standalone_requires_auth_before_binding() {
    // Reserve the address: the old entry point reaches bind and fails there;
    // the authenticated entry point must refuse earlier, without opening it.
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    for private in [false, true] {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_pstore-server"));
        cmd.env_clear()
            .env("PSTORE_LANE", "1")
            .env("PSTORE_BACKEND", "memory")
            .env("PSTORE_BIND", reserved.local_addr().unwrap().to_string());
        if private {
            cmd.env("PSTORE_ALLOW_UNAUTHENTICATED", "1");
        }
        let got = cmd.output().unwrap();
        assert!(!got.status.success());
        let stderr = String::from_utf8(got.stderr).unwrap();
        if private {
            assert!(stderr.contains("cannot bind"), "{stderr}");
        } else {
            assert!(stderr.contains("authentication"), "{stderr}");
            assert!(!stderr.contains("cannot bind"), "{stderr}");
        }
    }
}

#[tokio::test]
async fn a_bare_api_refuses_http_before_engine_access() {
    let store = Accounted::new(MemoryStore::new());
    let api = Api::new(store.clone(), LaneId(1)).unwrap();
    for (method, path, body) in [
        (
            "PUT",
            "/v1/indexes/docs/documents",
            r#"{"documents":[{"id":"a","vector":[1]}]}"#,
        ),
        ("GET", "/v1/indexes", ""),
        ("GET", "/metrics", ""),
        ("POST", "/v1/admin/fold", ""),
        ("POST", "/v1/internal/part", "{}"),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("x-pstore-tenant", "9")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let res = api.clone().router().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
        assert!(res.headers().contains_key("www-authenticate"));
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "unauthenticated");
        assert_eq!(body["error"]["retryable"], false);
    }
    for class in [
        OpClass::Read,
        OpClass::Write,
        OpClass::List,
        OpClass::Delete,
    ] {
        assert_eq!(
            store.total(class),
            0,
            "unauthenticated access issued {class:?}"
        );
    }
}
