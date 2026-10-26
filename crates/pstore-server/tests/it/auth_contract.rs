//! Tenant identity, endpoint scopes and source authority through the HTTP boundary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use crate::support;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use jsonwebtoken::Algorithm;
use pstore_server::Auth;
use serde_json::{Value, json};
use support::auth::*;
use tower::ServiceExt;

#[tokio::test]
async fn bearer_identity_is_bound_before_dispatch() {
    let (api, store) = api(&config(&[("current", &KEY)], "current"));
    let write = token("9", &["write"]);
    let good = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&write),
        None,
        &json!({"documents":[{"id":"a","vector":[1]}]}),
    )
    .await;
    assert_eq!(good.status, StatusCode::OK, "{}", good.text);
    let before = requests(&store);
    for tenant in ["10", "not-a-tenant", "-1"] {
        let got = send(
            &api,
            "PUT",
            "/v1/indexes/other/documents",
            Some(&write),
            Some(tenant),
            &json!({"documents":[{"id":"b","vector":[1]}]}),
        )
        .await;
        assert_eq!(got.status, StatusCode::FORBIDDEN, "{}", got.text);
        assert_eq!(got.body["error"]["code"], "forbidden");
        assert_eq!(requests(&store), before);
    }
    let read = token("9", &["read"]);
    let got = send(
        &api,
        "POST",
        "/v1/indexes/docs/query",
        Some(&read),
        Some("9"),
        &json!({"vector":[1]}),
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.text);
    assert_eq!(got.body["results"][0]["id"], "a");
    let high = token(&u128::MAX.to_string(), &["write"]);
    let got = send(
        &api,
        "PUT",
        "/v1/indexes/docs/documents",
        Some(&high),
        None,
        &json!({"documents":[{"id":"high","vector":[1]}]}),
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.text);
    let got = send(
        &api,
        "POST",
        "/v1/indexes/docs/query",
        Some(&read),
        None,
        &json!({"vector":[1]}),
    )
    .await;
    assert_eq!(got.body["results"][0]["id"], "a");
}

#[tokio::test]
async fn invalid_bearers_are_refused_before_blob_access() {
    let (api, store) = api(&config(&[("current", &KEY)], "current"));
    let base = claims("9", &["read"]);
    let mut variants = vec![
        sign(&base, Some("current"), Algorithm::HS256, &OTHER_KEY),
        sign(&base, Some("current"), Algorithm::HS384, &KEY),
        sign(&base, Some("unknown"), Algorithm::HS256, &KEY),
        sign(&base, None, Algorithm::HS256, &KEY),
        "not.a.jwt".to_owned(),
    ];
    for (field, value) in [
        ("iss", json!("other")),
        ("aud", json!("other")),
        ("exp", json!(1)),
        ("nbf", json!(u64::MAX)),
        ("tenant", json!("-1")),
        ("tenant", json!(9)),
        ("scopes", json!(["unknown"])),
        ("sub", json!("x".repeat(9000))),
    ] {
        let mut c = base.clone();
        c[field] = value;
        variants.push(sign(&c, Some("current"), Algorithm::HS256, &KEY));
    }
    for field in ["iss", "aud", "exp", "tenant", "scopes"] {
        let mut c = base.clone();
        c.as_object_mut().unwrap().remove(field);
        variants.push(sign(&c, Some("current"), Algorithm::HS256, &KEY));
    }
    for bearer in variants {
        let got = send(
            &api,
            "GET",
            "/v1/indexes/docs",
            Some(&bearer),
            Some("9"),
            &Value::Null,
        )
        .await;
        assert_eq!(got.status, StatusCode::UNAUTHORIZED, "{}", got.text);
        assert_eq!(got.body["error"]["code"], "unauthenticated");
        assert_eq!(got.body["error"]["retryable"], false);
        assert!(got.headers.contains_key("www-authenticate"));
        assert!(!got.text.contains(&bearer));
        assert_eq!(requests(&store), [0; 4]);
    }
}

#[tokio::test]
async fn route_scopes_fence_every_public_operation() {
    for (method, path, required) in [
        ("GET", "/v1/indexes", "list"),
        ("GET", "/v1/indexes/docs", "read"),
        ("DELETE", "/v1/indexes/docs", "write"),
        ("PUT", "/v1/indexes/docs/documents", "write"),
        ("POST", "/v1/indexes/docs/query", "read"),
        ("POST", "/v1/indexes/docs/warm", "read"),
        ("PUT", "/v1/indexes/docs/external", "write"),
        ("DELETE", "/v1/indexes/docs/external", "write"),
        ("POST", "/v1/indexes/docs/external/query", "read"),
        ("PUT", "/v1/indexes/docs/external/copy", "write"),
        ("GET", "/v1/indexes/docs/quarantine", "read"),
        ("DELETE", "/v1/indexes/docs/quarantine", "write"),
        ("GET", "/v1/replications", "list"),
        ("PUT", "/v1/indexes/docs/replication", "write"),
        ("GET", "/v1/indexes/docs/replication", "read"),
        ("DELETE", "/v1/indexes/docs/replication", "write"),
        ("POST", "/v1/indexes/docs/replication/pause", "write"),
        ("POST", "/v1/indexes/docs/replication/resume", "write"),
        ("PATCH", "/v1/indexes/docs/schema", "write"),
        ("POST", "/v1/admin/fold", "admin"),
        ("POST", "/v1/admin/gc", "admin"),
        ("GET", "/v1/admin/duties", "admin"),
        ("GET", "/metrics", "admin"),
    ] {
        let (api, store) = api(&config(&[("current", &KEY)], "current"));
        let others: Vec<&str> = ["read", "write", "list", "admin", "internal"]
            .into_iter()
            .filter(|s| *s != required)
            .collect();
        let denied = token("9", &others);
        let got = send(&api, method, path, Some(&denied), None, &json!({})).await;
        assert_eq!(
            got.status,
            StatusCode::FORBIDDEN,
            "{method} {path}: {}",
            got.text
        );
        assert_eq!(got.body["error"]["code"], "forbidden");
        assert_eq!(requests(&store), [0; 4]);
        let allowed = token("9", &[required]);
        let got = send(&api, method, path, Some(&allowed), None, &json!({})).await;
        assert_ne!(
            got.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path}: {}",
            got.text
        );
        assert_ne!(
            got.body["error"]["code"], "forbidden",
            "{method} {path}: {}",
            got.text
        );
    }
}

#[tokio::test]
async fn duplicate_credentials_and_conflicting_tenant_headers_are_refused() {
    let (api, store) = api(&config(&[("current", &KEY)], "current"));
    let bearer = token("9", &["read"]);
    for duplicate_auth in [true, false] {
        let mut req = Request::builder()
            .uri("/v1/indexes/docs")
            .header("authorization", format!("Bearer {bearer}"))
            .header("x-pstore-tenant", "9")
            .body(Body::empty())
            .unwrap();
        if duplicate_auth {
            req.headers_mut()
                .append("authorization", format!("Bearer {bearer}").parse().unwrap());
        } else {
            req.headers_mut()
                .append("x-pstore-tenant", "10".parse().unwrap());
        }
        let res = api.clone().router().oneshot(req).await.unwrap();
        assert_eq!(
            res.status(),
            if duplicate_auth {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            }
        );
        assert_eq!(requests(&store), [0; 4]);
    }
}

#[tokio::test]
async fn overlapping_keys_and_audience_arrays_are_supported() {
    let old = token("9", &["read"]);
    let mut c = claims("9", &["read"]);
    c["aud"] = json!(["other", "pstore-test-api"]);
    let new = sign(&c, Some("new"), Algorithm::HS256, &OTHER_KEY);
    let (both, _) = api(&config(&[("current", &KEY), ("new", &OTHER_KEY)], "new"));
    for token in [&old, &new] {
        let got = send(
            &both,
            "GET",
            "/v1/indexes/docs",
            Some(token),
            None,
            &Value::Null,
        )
        .await;
        assert_eq!(got.status, StatusCode::NOT_FOUND, "{}", got.text);
    }
    let (rotated, store) = api(&config(&[("new", &OTHER_KEY)], "new"));
    let got = send(
        &rotated,
        "GET",
        "/v1/indexes/docs",
        Some(&old),
        None,
        &Value::Null,
    )
    .await;
    assert_eq!(got.status, StatusCode::UNAUTHORIZED);
    assert_eq!(requests(&store), [0; 4]);
    let got = send(
        &rotated,
        "GET",
        "/v1/indexes/docs",
        Some(&new),
        None,
        &Value::Null,
    )
    .await;
    assert_eq!(got.status, StatusCode::NOT_FOUND);
}

#[test]
fn auth_configuration_is_explicit_bounded_and_redacted() {
    assert!(Auth::from_vars(|_| None).is_err());
    assert!(
        Auth::from_vars(|n| (n == "PSTORE_ALLOW_UNAUTHENTICATED").then(|| "1".to_owned())).is_ok()
    );
    let good = config(&[("current", &KEY)], "current");
    assert!(
        Auth::from_vars(|n| match n {
            "PSTORE_AUTH_CONFIG" => Some(good.to_string()),
            "PSTORE_ALLOW_UNAUTHENTICATED" => Some("1".to_owned()),
            _ => None,
        })
        .is_err()
    );
    let secret = base64::engine::general_purpose::STANDARD.encode(KEY);
    assert!(!format!("{:?}", auth(&good)).contains(&secret));
    let mut bads = vec![
        config(&[], "current"),
        config(&[("current", &KEY), ("current", &OTHER_KEY)], "current"),
        config(&[("current", &KEY)], "missing"),
        config(&[("current", &[1; 8])], "current"),
    ];
    let mut unknown = good.clone();
    unknown["typo"] = json!(secret);
    bads.push(unknown);
    let mut malformed = good.clone();
    malformed["keys"][0]["secret"] = json!("invalid secret");
    bads.push(malformed);
    for (field, value) in [
        ("issuer", json!("")),
        ("audience", json!("")),
        ("issuer", json!("i".repeat(1025))),
        ("active_kid", json!("k".repeat(65))),
        ("active_kid", json!("non ascii \u{00e9}")),
        (
            "keys",
            json!(
                (0..9)
                    .map(|i| json!({"kid":format!("k{i}"), "secret":secret}))
                    .collect::<Vec<_>>()
            ),
        ),
    ] {
        let mut bad = good.clone();
        bad[field] = value;
        bads.push(bad);
    }
    for bad in bads {
        let err =
            Auth::from_vars(|n| (n == "PSTORE_AUTH_CONFIG").then(|| bad.to_string())).unwrap_err();
        assert!(!format!("{err:?} {err}").contains(&secret));
        assert!(!format!("{err:?} {err}").contains("invalid secret"));
    }
    for private in ["0", "true", "yes", "2"] {
        assert!(
            Auth::from_vars(|n| (n == "PSTORE_ALLOW_UNAUTHENTICATED").then(|| private.to_owned()))
                .is_err()
        );
    }
}

#[tokio::test]
async fn legacy_http_parts_do_not_execute_for_any_policy() {
    let (secured, store) = api(&config(&[("current", &KEY)], "current"));
    for scopes in [vec!["internal"], vec!["read"], vec!["admin", "internal"]] {
        let got = send(
            &secured,
            "POST",
            "/v1/internal/part",
            Some(&token("9", &scopes)),
            None,
            &json!({"protocol":1}),
        )
        .await;
        assert_eq!(got.status, StatusCode::NOT_FOUND, "{}", got.text);
        assert_eq!(requests(&store), [0; 4]);
    }
    let private_store = pstore_blob::Accounted::new(pstore_blob::MemoryStore::new());
    let private =
        pstore_server::Api::unsecured(private_store.clone(), pstore_types::LaneId(1)).unwrap();
    let got = send(
        &private,
        "POST",
        "/v1/internal/part",
        None,
        Some("9"),
        &json!({"protocol":1}),
    )
    .await;
    assert_eq!(got.status, StatusCode::NOT_FOUND, "{}", got.text);
    assert_eq!(requests(&private_store), [0; 4]);
}
