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
    assert_eq!(d.tenant_flush_bytes(), 8 * 1024 * 1024);
    assert_eq!(d.tenant_buffered_bytes(), 64 * 1024 * 1024);
    assert_eq!(d.json_depth(), 128);
    assert_eq!(d.array_elements(), 16_384);
    assert_eq!(d.object_members(), 1024);
    assert_eq!(d.string_bytes(), 256 * 1024);
    assert_eq!(read(None), Ok(d));
    assert_eq!(read(Some(r#"{"version":1}"#)), Ok(d));
    assert_eq!(RequestLimits::builder().build(), Ok(d));
}

#[test]
fn configuration_lowers_each_limit() {
    let got = read(Some(
        r#"{"version":1,"body_bytes":100,"active_requests":4,"tenant_active_requests":2,
            "reserved_body_bytes":300,"body_read_timeout_ms":250,
            "tenant_flush_bytes":1000,"tenant_buffered_bytes":5000,
            "json_depth":3,"array_elements":10,"object_members":20,"string_bytes":30}"#,
    ))
    .unwrap();
    assert_eq!(got.body_bytes(), 100);
    assert_eq!(got.active_requests(), 4);
    assert_eq!(got.tenant_active_requests(), 2);
    assert_eq!(got.reserved_body_bytes(), 300);
    assert_eq!(got.body_read_timeout(), Duration::from_millis(250));
    assert_eq!(got.tenant_flush_bytes(), 1000);
    assert_eq!(got.tenant_buffered_bytes(), 5000);
    assert_eq!(got.json_depth(), 3);
    assert_eq!(got.array_elements(), 10);
    assert_eq!(got.object_members(), 20);
    assert_eq!(got.string_bytes(), 30);
    let same = RequestLimits::builder()
        .tenant_flush_bytes(7)
        .tenant_buffered_bytes(7)
        .build()
        .unwrap();
    assert_eq!(same.tenant_buffered_bytes(), same.tenant_flush_bytes());
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
        r#"{"version":1,"array_element":10}"#,
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
        (
            r#"{"version":1,"tenant_flush_bytes":0}"#,
            LimitsError::Zero("tenant_flush_bytes"),
        ),
        (
            r#"{"version":1,"tenant_buffered_bytes":0}"#,
            LimitsError::Zero("tenant_buffered_bytes"),
        ),
        (
            r#"{"version":1,"tenant_flush_bytes":8388609}"#,
            LimitsError::AboveDefault("tenant_flush_bytes"),
        ),
        (
            r#"{"version":1,"tenant_buffered_bytes":67108865}"#,
            LimitsError::AboveDefault("tenant_buffered_bytes"),
        ),
        (
            r#"{"version":1,"tenant_flush_bytes":100,"tenant_buffered_bytes":99}"#,
            LimitsError::BufferBelowFlush,
        ),
        (
            r#"{"version":1,"json_depth":0}"#,
            LimitsError::Zero("json_depth"),
        ),
        (
            r#"{"version":1,"array_elements":0}"#,
            LimitsError::Zero("array_elements"),
        ),
        (
            r#"{"version":1,"object_members":0}"#,
            LimitsError::Zero("object_members"),
        ),
        (
            r#"{"version":1,"string_bytes":0}"#,
            LimitsError::Zero("string_bytes"),
        ),
        (
            r#"{"version":1,"json_depth":129}"#,
            LimitsError::AboveDefault("json_depth"),
        ),
        (
            r#"{"version":1,"array_elements":16385}"#,
            LimitsError::AboveDefault("array_elements"),
        ),
        (
            r#"{"version":1,"object_members":1025}"#,
            LimitsError::AboveDefault("object_members"),
        ),
        (
            r#"{"version":1,"string_bytes":262145}"#,
            LimitsError::AboveDefault("string_bytes"),
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
            // M128's fixed cap, pinned since M280 made borrowing the default.
            .tenant_borrow_reserve(None)
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

#[test]
fn query_row_limits_follow_m128s_rules() {
    let d = RequestLimits::default();
    assert_eq!(d.query_rows(), 4_194_304);
    assert_eq!(d.tenant_query_rows(), 1_048_576);
    let got = read(Some(
        r#"{"version":1,"query_rows":900,"tenant_query_rows":300}"#,
    ))
    .unwrap();
    assert_eq!((got.query_rows(), got.tenant_query_rows()), (900, 300));
    // A tenant may hold the whole pool.
    let edge = read(Some(
        r#"{"version":1,"query_rows":5,"tenant_query_rows":5}"#,
    ))
    .unwrap();
    assert_eq!(edge.tenant_query_rows(), 5);
    for (raw, want) in [
        (
            r#"{"version":1,"query_rows":0}"#,
            LimitsError::Zero("query_rows"),
        ),
        (
            r#"{"version":1,"tenant_query_rows":0}"#,
            LimitsError::Zero("tenant_query_rows"),
        ),
        (
            r#"{"version":1,"query_rows":4194305}"#,
            LimitsError::AboveDefault("query_rows"),
        ),
        (
            r#"{"version":1,"tenant_query_rows":1048577}"#,
            LimitsError::AboveDefault("tenant_query_rows"),
        ),
        (
            r#"{"version":1,"query_rows":10,"tenant_query_rows":11}"#,
            LimitsError::TenantRowsAboveGlobal,
        ),
    ] {
        assert_eq!(read(Some(raw)), Err(want), "{raw}");
    }
}

#[test]
fn a_query_reservation_is_bounded_per_tenant_and_in_all_and_returned_on_drop() {
    let limits = RequestLimits::builder()
        .query_rows(10)
        .tenant_query_rows(6)
        .build()
        .unwrap();
    let ingress = Arc::new(Ingress::new(limits));
    let (a, b) = (TenantId(1), TenantId(2));
    // Never fits: too large, whatever is held.
    assert_eq!(
        ingress.reserve_rows(a, 7).err().unwrap().status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let r1 = ingress.reserve_rows(a, 6).unwrap();
    assert_eq!(ingress.reserved_rows(), 6);
    // The tenant's own pool is full: overloaded, with when to retry.
    let refused = ingress.reserve_rows(a, 1).err().unwrap();
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.retry_after, Some(1));
    // Another tenant fits until the server's pool is full.
    let r2 = ingress.reserve_rows(b, 4).unwrap();
    assert_eq!(
        ingress.reserve_rows(b, 1).err().unwrap().status,
        StatusCode::TOO_MANY_REQUESTS
    );
    drop(r1);
    assert_eq!(ingress.reserved_rows(), 4);
    let r3 = ingress.reserve_rows(b, 2).unwrap();
    assert_eq!(ingress.reserved_rows(), 6);
    drop((r2, r3));
    assert_eq!(ingress.reserved_rows(), 0);
    assert!(
        ingress.lock().tenant_rows.is_empty(),
        "an idle tenant leaves no entry"
    );
}

/// M280: one admission workload, run step by step against an [`Ingress`].
///
/// Each step: requests that finished release their slots; the hot tenant (`Some(hold)`) tries
/// to fill the server, each request held `hold` steps; then `small(step)` small tenants arrive,
/// each with one request held `SMALL_HOLD` steps. The hot tenant goes first, which is the
/// pessimistic order for borrowing: it has taken every slot it may before a small one arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Admitted {
    /// Small tenants' requests refused, of those offered.
    small_refused: usize,
    small_offered: usize,
    /// The hot tenant's requests admitted, summed over the steps they were held.
    hot_slot_steps: usize,
    /// Every admitted request's slot-steps.
    used_slot_steps: usize,
}

const STEPS: usize = 200;
const SMALL_HOLD: usize = 5;

fn simulate(
    reserve: Option<usize>,
    hot: Option<usize>,
    small: impl Fn(usize) -> usize,
) -> Admitted {
    let ingress = Arc::new(Ingress::new(
        RequestLimits::builder()
            .tenant_borrow_reserve(reserve)
            .build()
            .unwrap(),
    ));
    let mut held: Vec<(usize, Permit, bool)> = Vec::new();
    let mut out = Admitted {
        small_refused: 0,
        small_offered: 0,
        hot_slot_steps: 0,
        used_slot_steps: 0,
    };
    let mut next_small = 1000u128;
    for step in 0..STEPS {
        held.retain(|(until, _, _)| *until > step);
        if let Some(hold) = hot {
            while let Some(p) = ingress.admit(Key::Tenant(TenantId(1))) {
                held.push((step + hold, p, true));
            }
        }
        for _ in 0..small(step) {
            out.small_offered += 1;
            next_small += 1;
            match ingress.admit(Key::Tenant(TenantId(next_small))) {
                Some(p) => held.push((step + SMALL_HOLD, p, false)),
                None => out.small_refused += 1,
            }
        }
        out.used_slot_steps += held.len();
        out.hot_slot_steps += held.iter().filter(|(_, _, hot)| *hot).count();
    }
    out
}

/// A workload: its name, the hot tenant's hold (if any), and the small tenants a step.
type Workload = (&'static str, Option<usize>, Box<dyn Fn(usize) -> usize>);

/// The workloads M280's default is chosen from, by name.
fn workloads() -> Vec<Workload> {
    vec![
        ("hot tenant alone", Some(10), Box::new(|_| 0)),
        ("hot + 1 small a step", Some(10), Box::new(|_| 1)),
        (
            "hot + bursts of 8 small",
            Some(10),
            Box::new(|s| if s % 20 == 0 { 8 } else { 0 }),
        ),
        (
            "hot + bursts of 24 small",
            Some(10),
            Box::new(|s| if s % 20 == 0 { 24 } else { 0 }),
        ),
        ("small tenants only, 10 a step", None, Box::new(|_| 10)),
    ]
}

#[test]
fn borrowing_admission_measured_against_the_fixed_cap() {
    // ⚠️ The numbers M280's default rests on; exact, because the simulation is deterministic.
    let mut rows = Vec::new();
    for (name, hot, small) in workloads() {
        let fixed = simulate(None, hot, &small);
        let borrow = simulate(Some(8), hot, &small);
        println!("{name}: fixed {fixed:?}\n{name}: borrow {borrow:?}");
        rows.push((name, fixed, borrow));
    }
    let hot = |a: &Admitted| a.hot_slot_steps / STEPS;
    let refused = |a: &Admitted| (a.small_refused, a.small_offered);
    // The hot tenant alone: 8 slots fixed, 56 borrowing (64 less the reserve of 8).
    assert_eq!((hot(&rows[0].1), hot(&rows[0].2)), (8, 56));
    // A trickle of small tenants fits inside the reserve: none refused either way.
    assert_eq!(refused(&rows[1].1), (0, 200));
    assert_eq!(refused(&rows[1].2), (0, 200));
    // A burst the reserve holds: none refused either way.
    assert_eq!(refused(&rows[2].2), (0, 80));
    // ⚠️ The downside: a burst larger than the reserve, against a hot tenant that has borrowed
    // every slot it may, is refused past the reserve, where the fixed cap refused nothing.
    assert_eq!(refused(&rows[3].1), (0, 240));
    assert_eq!(refused(&rows[3].2), (160, 240));
    // Small tenants alone never reach their own cap, so the modes agree.
    assert_eq!(rows[4].1, rows[4].2);
}

#[test]
fn the_fixed_cap_is_m128s_and_borrowing_keeps_its_reserve() {
    let fixed = Arc::new(Ingress::new(
        RequestLimits::builder()
            .active_requests(6)
            .tenant_active_requests(2)
            .tenant_borrow_reserve(None)
            .build()
            .unwrap(),
    ));
    let a = Key::Tenant(TenantId(1));
    let held: Vec<Permit> = std::iter::from_fn(|| fixed.admit(a)).collect();
    assert_eq!(held.len(), 2, "M128: a tenant's own cap, whatever is free");
    let borrow = Arc::new(Ingress::new(
        RequestLimits::builder()
            .active_requests(6)
            .tenant_active_requests(2)
            .tenant_borrow_reserve(Some(2))
            .build()
            .unwrap(),
    ));
    let held: Vec<Permit> = std::iter::from_fn(|| borrow.admit(a)).collect();
    assert_eq!(
        held.len(),
        4,
        "its cap of 2, then 2 borrowed, leaving the reserve of 2"
    );
    let b = Key::Tenant(TenantId(2));
    let small: Vec<Permit> = std::iter::from_fn(|| borrow.admit(b)).collect();
    assert_eq!(
        small.len(),
        2,
        "the reserve admits a tenant within its own cap"
    );
    assert!(
        borrow.admit(Key::Tenant(TenantId(3))).is_none(),
        "the server is full"
    );
    drop(held);
    assert_eq!(borrow.counts().active, 2);
}

#[test]
fn borrowing_is_configured_off_on_or_by_its_reserve() {
    let d = RequestLimits::default();
    assert_eq!(d.tenant_borrow_reserve(), Some(8), "on, measured: see M280");
    let got = |raw: &str| read(Some(raw)).map(|l| l.tenant_borrow_reserve());
    assert_eq!(got(r#"{"version":1,"tenant_borrowing":false}"#), Ok(None));
    assert_eq!(
        got(r#"{"version":1,"tenant_borrowing":false,"tenant_borrow_reserve":4}"#),
        Ok(None),
        "off wins"
    );
    assert_eq!(
        got(r#"{"version":1,"tenant_borrowing":true}"#),
        Ok(Some(
            DEFAULT_BORROW_RESERVE.unwrap_or(BORROW_RESERVE_WHEN_ON)
        ))
    );
    assert_eq!(
        got(r#"{"version":1,"tenant_borrow_reserve":4}"#),
        Ok(Some(4))
    );
    assert_eq!(got(r#"{"version":1}"#), Ok(DEFAULT_BORROW_RESERVE));
    assert_eq!(
        got(r#"{"version":1,"active_requests":8,"tenant_borrow_reserve":8}"#),
        Ok(Some(8))
    );
}

#[test]
fn a_reserve_the_server_cannot_leave_free_is_the_fixed_cap() {
    // 4 slots, a reserve of 4 (the default 8 is past them too): nobody may borrow.
    for reserve in [Some(4), Some(8)] {
        let ingress = Arc::new(Ingress::new(
            RequestLimits::builder()
                .active_requests(4)
                .tenant_active_requests(2)
                .tenant_borrow_reserve(reserve)
                .build()
                .unwrap(),
        ));
        let a = Key::Tenant(TenantId(1));
        let held: Vec<Permit> = std::iter::from_fn(|| ingress.admit(a)).collect();
        assert_eq!(held.len(), 2, "{reserve:?}");
    }
}
