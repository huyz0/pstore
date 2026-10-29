#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use super::*;

fn read(raw: Option<&str>) -> Result<RequestLimits, LimitsError> {
    RequestLimits::from_vars(|k| {
        (k == "PSTORE_REQUEST_LIMITS_CONFIG")
            .then_some(raw)
            .flatten()
            .map(str::to_owned)
    })
}

#[test]
fn version_one_defaults_are_the_documented_contract() {
    let d = RequestLimits::default();
    assert_eq!(d.body_bytes(), 2 * 1024 * 1024);
    assert_eq!(d.active_requests(), 64);
    assert_eq!(d.tenant_active_requests(), 8);
    assert_eq!(d.reserved_body_bytes(), 128 * 1024 * 1024);
    assert_eq!(d.body_read_timeout(), Duration::from_secs(30));
    assert_eq!(read(None), Ok(d));
    assert_eq!(read(Some(r#"{"version":1}"#)), Ok(d));
    assert_eq!(RequestLimits::builder().build(), Ok(d));
}

#[test]
fn configuration_lowers_each_limit() {
    let got = read(Some(
        r#"{"version":1,"body_bytes":100,"active_requests":4,"tenant_active_requests":2,
            "reserved_body_bytes":300,"body_read_timeout_ms":250}"#,
    ))
    .unwrap();
    assert_eq!(got.body_bytes(), 100);
    assert_eq!(got.active_requests(), 4);
    assert_eq!(got.tenant_active_requests(), 2);
    assert_eq!(got.reserved_body_bytes(), 300);
    assert_eq!(got.body_read_timeout(), Duration::from_millis(250));
    // The tenant may hold every global slot, and the pool exactly one body.
    let edge = RequestLimits::builder()
        .active_requests(3)
        .tenant_active_requests(3)
        .body_bytes(7)
        .reserved_body_bytes(7)
        .build()
        .unwrap();
    assert_eq!(edge.tenant_active_requests(), 3);
    assert_eq!(edge.reserved_body_bytes(), 7);
}

#[test]
fn unsafe_or_malformed_configuration_is_refused() {
    for raw in [
        "",
        "{}",
        "[]",
        r#"{"version":2}"#,
        r#"{"version":"1"}"#,
        r#"{"version":1,"body_byte":10}"#,
        r#"{"version":1,"body_bytes":-1}"#,
        r#"{"version":1,"body_bytes":1.5}"#,
        r#"{"version":1,"body_bytes":18446744073709551616}"#,
        r#"{"version":1,"body_bytes":10,"body_bytes":11}"#,
    ] {
        assert!(
            matches!(read(Some(raw)), Err(LimitsError::Malformed(_))),
            "{raw}"
        );
    }
    assert!(matches!(
        read(Some(&format!(
            r#"{{"version":1{}}}"#,
            " ".repeat(MAX_CONFIG)
        ))),
        Err(LimitsError::Malformed(_))
    ));
    // Exactly the maximum length is still read.
    let longest = format!(r#"{{"version":1{}}}"#, " ".repeat(MAX_CONFIG - 13));
    assert_eq!(longest.len(), MAX_CONFIG);
    assert_eq!(read(Some(&longest)), Ok(RequestLimits::default()));
    for (raw, want) in [
        (
            r#"{"version":1,"body_bytes":0}"#,
            LimitsError::Zero("body_bytes"),
        ),
        (
            r#"{"version":1,"active_requests":0}"#,
            LimitsError::Zero("active_requests"),
        ),
        (
            r#"{"version":1,"tenant_active_requests":0}"#,
            LimitsError::Zero("tenant_active_requests"),
        ),
        (
            r#"{"version":1,"reserved_body_bytes":0}"#,
            LimitsError::Zero("reserved_body_bytes"),
        ),
        (
            r#"{"version":1,"body_read_timeout_ms":0}"#,
            LimitsError::Zero("body_read_timeout_ms"),
        ),
        (
            r#"{"version":1,"body_bytes":2097153}"#,
            LimitsError::AboveDefault("body_bytes"),
        ),
        (
            r#"{"version":1,"active_requests":65}"#,
            LimitsError::AboveDefault("active_requests"),
        ),
        (
            r#"{"version":1,"tenant_active_requests":9}"#,
            LimitsError::AboveDefault("tenant_active_requests"),
        ),
        (
            r#"{"version":1,"reserved_body_bytes":134217729}"#,
            LimitsError::AboveDefault("reserved_body_bytes"),
        ),
        (
            r#"{"version":1,"body_read_timeout_ms":30001}"#,
            LimitsError::AboveDefault("body_read_timeout_ms"),
        ),
        (
            r#"{"version":1,"active_requests":2,"tenant_active_requests":3}"#,
            LimitsError::TenantAboveGlobal,
        ),
        (
            r#"{"version":1,"body_bytes":100,"reserved_body_bytes":99}"#,
            LimitsError::PoolBelowBody,
        ),
    ] {
        assert_eq!(read(Some(raw)), Err(want), "{raw}");
    }
}

#[test]
fn content_metadata_is_read_strictly() {
    let mut h = HeaderMap::new();
    assert!(identity_encoded(&h));
    for ok in ["identity", "IDENTITY", " identity , identity"] {
        h.insert(header::CONTENT_ENCODING, ok.parse().unwrap());
        assert!(identity_encoded(&h), "{ok}");
    }
    for bad in ["gzip", "identity, br", "deflate"] {
        h.insert(header::CONTENT_ENCODING, bad.parse().unwrap());
        assert!(!identity_encoded(&h), "{bad}");
    }
    h.insert(
        header::CONTENT_ENCODING,
        axum::http::HeaderValue::from_bytes(b"\xff").unwrap(),
    );
    assert!(!identity_encoded(&h));
    let mut h = HeaderMap::new();
    assert!(!declared_over(&h, 10));
    h.insert(header::CONTENT_LENGTH, "10".parse().unwrap());
    assert!(!declared_over(&h, 10));
    h.append(header::CONTENT_LENGTH, "11".parse().unwrap());
    assert!(
        declared_over(&h, 10),
        "any declared length over the limit refuses"
    );
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_LENGTH, "nonsense".parse().unwrap());
    assert!(
        !declared_over(&h, 10),
        "a length that is not one is never trusted"
    );
}

#[test]
fn a_permit_returns_everything_and_idle_tenants_leave_no_entry() {
    let ingress = Arc::new(Ingress::new(
        RequestLimits::builder()
            .active_requests(3)
            .tenant_active_requests(2)
            .body_bytes(10)
            .reserved_body_bytes(30)
            .build()
            .unwrap(),
    ));
    let a = Key::Tenant(TenantId(1));
    let p1 = ingress.admit(a).unwrap();
    let p2 = ingress.admit(a).unwrap();
    assert!(ingress.admit(a).is_none(), "a tenant's own allowance");
    let p3 = ingress.admit(Key::Private).unwrap();
    assert!(ingress.admit(Key::Tenant(TenantId(2))).is_none(), "global");
    assert_eq!(
        ingress.counts(),
        IngressCounts {
            active: 3,
            reserved_body_bytes: 30,
            tenants: 2
        }
    );
    drop(p1);
    assert_eq!(ingress.counts().tenants, 2);
    drop(p2);
    drop(p3);
    assert_eq!(ingress.counts(), IngressCounts::default());
    // The byte pool, alone, can be what refuses.
    let pool = Arc::new(Ingress::new(
        RequestLimits::builder()
            .active_requests(4)
            .tenant_active_requests(4)
            .body_bytes(10)
            .reserved_body_bytes(19)
            .build()
            .unwrap(),
    ));
    let held = pool.admit(a).unwrap();
    assert!(pool.admit(Key::Tenant(TenantId(2))).is_none());
    drop(held);
    assert!(pool.admit(Key::Tenant(TenantId(2))).is_some());
}

#[test]
fn a_poisoned_count_is_recovered() {
    let ingress = Arc::new(Ingress::new(RequestLimits::default()));
    let p = ingress.admit(Key::Private).unwrap();
    let poison = Arc::clone(&ingress);
    let _ = std::thread::spawn(move || {
        let _g = poison.counts.lock().unwrap();
        panic!("poison");
    })
    .join();
    drop(p);
    assert_eq!(ingress.counts(), IngressCounts::default());
    assert!(ingress.admit(Key::Private).is_some());
}

/// Unit tests for what M128's integration tests also see, so a mutant here fails in the
/// first test binary cargo runs (see `an_api_reports_the_limits_it_was_given`).
#[test]
fn an_ingress_keeps_its_limits_and_overload_says_when_to_retry() {
    let limits = RequestLimits::builder()
        .tenant_active_requests(2)
        .build()
        .unwrap();
    assert_eq!(Ingress::new(limits).limits(), &limits);
    let e = overloaded();
    assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(e.retry_after, Some(1));
}
