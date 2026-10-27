//! UTC expiration boundaries are a predicate, independent of scheduler timing.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]

#[test]
fn verification_enforces_the_injected_clock_at_expiry_and_not_before() {
    use super::*;
    use axum::http::header::AUTHORIZATION;
    let secret = [42_u8; 32];
    let config = serde_json::json!({
        "issuer":"issuer", "audience":"audience", "active_kid":"key",
        "keys":[{"kid":"key", "secret":base64::engine::general_purpose::STANDARD.encode(secret)}]
    });
    let auth =
        Auth::from_vars(|key| (key == "PSTORE_AUTH_CONFIG").then(|| config.to_string())).unwrap();
    let utc = now().unwrap();
    let exp = utc + 3600;
    let start = utc - 1000;
    let mut header = jsonwebtoken::Header::new(Algorithm::HS256);
    header.kid = Some("key".to_owned());
    let token = jsonwebtoken::encode(
        &header,
        &Claims {
            iss: "issuer".to_owned(),
            aud: Audience::One("audience".to_owned()),
            exp,
            nbf: Some(start),
            tenant: "9".to_owned(),
            scopes: vec![Scope::Read],
            sources: vec![],
        },
        &jsonwebtoken::EncodingKey::from_secret(&secret),
    )
    .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
    // The library's real-clock validation accepts this token. These controls
    // therefore isolate the stricter predicate at the verification call site.
    assert!(auth.verify(&headers).is_ok());
    for (clock, allowed) in [
        (exp - 1, true),
        (exp, false),
        (start - 1, false),
        (start, true),
    ] {
        assert_eq!(
            auth.verify_with_time(&headers, || Ok(clock)).is_ok(),
            allowed,
            "clock={clock}"
        );
    }
    assert!(auth.verify_with_time(&headers, || Err(AuthError)).is_err());
}

#[test]
fn expiry_is_strict_and_not_before_is_inclusive() {
    for (exp, nbf, now, allowed) in [
        (10, None, 9, true),
        (10, None, 10, false),
        (10, None, 11, false),
        (10, Some(5), 4, false),
        (10, Some(5), 5, true),
        (10, Some(20), 15, false),
        (0, None, 0, false),
        (u64::MAX, Some(u64::MAX - 1), u64::MAX - 1, true),
        (u64::MAX, None, u64::MAX, false),
    ] {
        assert_eq!(
            super::active_at(exp, nbf, now),
            allowed,
            "exp={exp}, nbf={nbf:?}, now={now}"
        );
    }
}

fn bounded_config(kid: &str, secret: usize, issuer: &str, audience: &str) -> serde_json::Value {
    use base64::Engine as _;
    serde_json::json!({"issuer":issuer,"audience":audience,"active_kid":kid,
        "keys":[{"kid":kid,"secret":base64::engine::general_purpose::STANDARD.encode(vec![37_u8;secret])}]})
}
fn configured(value: &serde_json::Value) -> Result<super::Auth, super::AuthError> {
    super::Auth::from_vars(|name| (name == "PSTORE_AUTH_CONFIG").then(|| value.to_string()))
}

#[test]
fn malformed_credentials_and_nonactive_key_entries_fail_closed() {
    use super::*;
    use axum::http::header::AUTHORIZATION;
    let auth = configured(&bounded_config("k0", 32, "issuer", "audience")).unwrap();
    assert!(auth.verify(&HeaderMap::new()).is_err());
    for credential in [
        "Bearer".to_owned(),
        "Bearer ".to_owned(),
        "Basic value".to_owned(),
        format!("Bearer {}", "a".repeat(8193)),
        "Bearer a.b.c".to_owned(),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&credential).unwrap());
        assert!(auth.verify(&headers).is_err());
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_bytes(b"Bearer \xff").unwrap(),
    );
    assert!(auth.verify(&headers).is_err());
    for kid in ["", "with space", &"k".repeat(65)] {
        let mut config = bounded_config("k0", 32, "issuer", "audience");
        let mut extra = config
            .get("keys")
            .unwrap()
            .as_array()
            .unwrap()
            .first()
            .unwrap()
            .clone();
        extra
            .as_object_mut()
            .unwrap()
            .insert("kid".to_owned(), serde_json::json!(kid));
        config
            .get_mut("keys")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(extra);
        assert!(configured(&config).is_err());
    }
    for (policy, expected) in [
        (Auth::closed(), "Auth(Closed)"),
        (Auth::unsecured(), "Auth(Unsecured)"),
    ] {
        assert_eq!(format!("{policy:?}"), expected);
    }
}

#[tokio::test]
async fn scoped_routes_refuse_missing_principals_before_dispatch() {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let route = Scope::Read.on(axum::routing::get(move || {
        observed.fetch_add(1, Ordering::SeqCst);
        async { "permitted" }
    }));
    let router = ScopedRouter::<()>::new(Arc::new(crate::ingress::Ingress::new(
        crate::ingress::RequestLimits::default(),
    )))
    .route("/", route)
    .finish();
    for (principal, expected) in [
        (None, StatusCode::FORBIDDEN),
        (
            Some(Principal::Verified {
                tenant: TenantId(9),
                scopes: vec![Scope::Write],
                sources: vec![],
            }),
            StatusCode::FORBIDDEN,
        ),
        (
            Some(Principal::Verified {
                tenant: TenantId(9),
                scopes: vec![Scope::Read],
                sources: vec![],
            }),
            StatusCode::OK,
        ),
        (Some(Principal::Unsecured), StatusCode::OK),
    ] {
        let mut request = Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap();
        if let Some(principal) = principal {
            request.extensions_mut().insert(principal);
        }
        let before = calls.load(Ordering::SeqCst);
        assert_eq!(
            router.clone().oneshot(request).await.unwrap().status(),
            expected
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before + usize::from(expected == StatusCode::OK)
        );
    }
}

#[test]
fn keyring_configuration_accepts_only_bounded_complete_values() {
    use base64::Engine as _;
    for (bytes, allowed) in [
        (0, false),
        (31, false),
        (32, true),
        (1024, true),
        (1025, false),
    ] {
        assert_eq!(
            configured(&bounded_config("k0", bytes, "issuer", "audience")).is_ok(),
            allowed,
            "secret bytes={bytes}"
        );
    }
    for (kid, allowed) in [
        (String::new(), false),
        ("with space".to_owned(), false),
        ("k".repeat(64), true),
        ("k".repeat(65), false),
    ] {
        assert_eq!(
            configured(&bounded_config(&kid, 32, "issuer", "audience")).is_ok(),
            allowed
        );
    }
    for (size, allowed) in [(0, false), (1024, true), (1025, false)] {
        let text = "i".repeat(size);
        assert_eq!(
            configured(&bounded_config("k0", 32, &text, "audience")).is_ok(),
            allowed
        );
        assert_eq!(
            configured(&bounded_config("k0", 32, "issuer", &text)).is_ok(),
            allowed
        );
    }
    for (count, allowed) in [(0, false), (8, true), (9, false)] {
        let keys: Vec<_>=(0..count).map(|index|serde_json::json!({"kid":format!("k{index}"),"secret":base64::engine::general_purpose::STANDARD.encode([37_u8;32])})).collect();
        let value = serde_json::json!({"issuer":"issuer","audience":"audience","active_kid":"k0","keys":keys});
        assert_eq!(configured(&value).is_ok(), allowed);
    }
    let mut absent = bounded_config("k0", 32, "issuer", "audience");
    absent
        .as_object_mut()
        .unwrap()
        .insert("active_kid".to_owned(), serde_json::json!("absent"));
    assert!(configured(&absent).is_err());
    let mut excessive = bounded_config("k0", 32, "issuer", "audience");
    excessive
        .get_mut("keys")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .first_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("secret".to_owned(), serde_json::json!("A".repeat(2049)));
    assert!(configured(&excessive).is_err());
    let mut padded = bounded_config("k0", 32, "issuer", "audience").to_string();
    padded.extend(std::iter::repeat_n(' ', 65_536 - padded.len()));
    assert!(
        super::Auth::from_vars(|key| (key == "PSTORE_AUTH_CONFIG").then(|| padded.clone())).is_ok()
    );
    padded.push(' ');
    assert!(
        super::Auth::from_vars(|key| (key == "PSTORE_AUTH_CONFIG").then(|| padded.clone()))
            .is_err()
    );
}

#[test]
fn signed_claim_limits_and_late_clock_failure_fail_closed() {
    use super::*;
    use axum::http::header::AUTHORIZATION;
    fn claims() -> Claims {
        Claims {
            iss: "issuer".to_owned(),
            aud: Audience::One("audience".to_owned()),
            exp: now().unwrap() + 3600,
            nbf: None,
            tenant: "9".to_owned(),
            scopes: vec![Scope::Read],
            sources: vec![],
        }
    }
    fn headers(claims: &Claims) -> HeaderMap {
        let mut header = jsonwebtoken::Header::new(Algorithm::HS256);
        header.kid = Some("k0".to_owned());
        let token = jsonwebtoken::encode(
            &header,
            claims,
            &jsonwebtoken::EncodingKey::from_secret(&[37_u8; 32]),
        )
        .unwrap();
        assert!(
            token.len() <= 8192,
            "the claim bound must be reached before the token byte bound"
        );
        let mut result = HeaderMap::new();
        result.insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        result
    }
    let auth = configured(&bounded_config("k0", 32, "issuer", "audience")).unwrap();
    for (count, allowed) in [(64, true), (65, false)] {
        let mut claim = claims();
        claim.sources = (0..count)
            .map(|index| Source {
                tenant: (index + 1).to_string(),
                store: String::new(),
            })
            .collect();
        assert_eq!(
            auth.verify(&headers(&claim)).is_ok(),
            allowed,
            "source count={count}"
        );
    }
    for (count, allowed) in [(5, true), (6, false)] {
        let mut claim = claims();
        claim.scopes = vec![Scope::Read; count];
        assert_eq!(
            auth.verify(&headers(&claim)).is_ok(),
            allowed,
            "scope count={count}"
        );
    }
    for (size, allowed) in [(256, true), (257, false)] {
        let mut claim = claims();
        claim.sources.push(Source {
            tenant: "10".to_owned(),
            store: "s".repeat(size),
        });
        assert_eq!(
            auth.verify(&headers(&claim)).is_ok(),
            allowed,
            "store bytes={size}"
        );
    }
    for (tenant, allowed) in [
        (u128::MAX.to_string(), true),
        (format!("{}0", u128::MAX), false),
        (String::new(), false),
        ("not-decimal".to_owned(), false),
    ] {
        let mut claim = claims();
        claim.sources.push(Source {
            tenant,
            store: String::new(),
        });
        assert_eq!(auth.verify(&headers(&claim)).is_ok(), allowed);
    }
    let header = headers(&claims());
    let called = std::cell::Cell::new(false);
    assert!(
        auth.verify_with_time(&header, || if called.replace(true) {
            Err(AuthError)
        } else {
            now()
        })
        .is_err()
    );
}
