//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! What the suite and the decorators do when the backend underneath is hostile.
//!
//! A conformance suite that panics on a broken backend cannot report on a broken backend,
//! which is the only situation it exists for.
use bytes::Bytes;
use pstore_blob::{BlobStore, Congested, Faults, Faulty, Key, MemoryStore, Support};
use pstore_testkit::conformance;

fn k(s: &str) -> Key {
    Key::new(s)
}

fn broken() -> Faulty<MemoryStore> {
    Faulty::new(
        MemoryStore::new(),
        1,
        Faults {
            read_error: 1.0,
            write_error: 1.0,
            ..Faults::none()
        },
    )
}

#[tokio::test]
async fn the_suite_survives_a_backend_that_fails_everything() {
    // It must report, not panic and not hang.
    let r = conformance::run(&broken(), 100).await;
    assert!(!r.conforms());
    assert_eq!(r.probes.len(), 9);
    assert!(
        r.probes.iter().any(|p| p.outcome == Support::Unsupported),
        "a backend that refuses every write is Unsupported, not merely Divergent"
    );
}

#[tokio::test]
async fn the_suite_survives_a_backend_that_only_fails_writes() {
    let s = Faulty::new(
        MemoryStore::new(),
        3,
        Faults {
            write_error: 1.0,
            ..Faults::none()
        },
    );
    let r = conformance::run(&s, 101).await;
    assert!(!r.conforms());
}

#[tokio::test]
async fn the_suite_survives_a_backend_that_only_fails_conditional_writes() {
    let s = Faulty::new(
        MemoryStore::new(),
        4,
        Faults {
            cas_lost: 1.0,
            ..Faults::none()
        },
    );
    let r = conformance::run(&s, 102).await;
    assert!(!r.conforms());
    // create_if_absent's FIRST write is refused, so the probe cannot even reach the
    // property it tests -- Unsupported, which is a different claim from Divergent.
    let cia = r
        .probes
        .iter()
        .find(|p| p.name == "create_if_absent")
        .unwrap();
    assert_eq!(cia.outcome, Support::Unsupported);
}

#[tokio::test]
async fn every_method_retries_a_transient_slowdown() {
    // The retry wrapper is applied per method; one of them forgetting it would only show
    // up as an unexplained 503 reaching a caller in production.
    let inner = MemoryStore::new();
    inner
        .put(&k("a"), Bytes::from_static(b"hello"))
        .await
        .unwrap();

    for n in 1..=6u32 {
        let flaky = Faulty::new(
            inner.clone(),
            1,
            Faults {
                slow_down_first_n: 1,
                ..Faults::none()
            },
        );
        let s = Congested::new(flaky, 8);
        match n {
            1 => drop(s.get(&k("a")).await.unwrap()),
            2 => drop(s.get_range(&k("a"), 0..2).await.unwrap()),
            3 => drop(s.head(&k("a")).await.unwrap()),
            4 => drop(s.put(&k("b"), Bytes::from_static(b"x")).await.unwrap()),
            5 => s.delete_batch(&[k("b")]).await.unwrap(),
            _ => drop(s.list_unrestricted(&k("")).await.unwrap()),
        }
        assert_eq!(s.attempts(), 2, "method {n} did not retry the first 503");
    }
}

#[tokio::test]
async fn a_non_slowdown_error_is_not_retried() {
    // Retrying a deterministic failure just burns the budget and delays the report.
    let s = Congested::new(broken(), 8);
    assert!(s.get(&k("a")).await.is_err());
    assert_eq!(s.attempts(), 1);
}

#[tokio::test]
async fn a_cas_failure_is_never_retried_by_the_transport() {
    // Deliberate: `Lost` means rebase against the new state and `Contended` means retry
    // the same attempt. Retrying blindly here would re-send a body built from a world
    // that no longer exists.
    use pstore_blob::{CasError, Precondition};
    let s = Congested::new(
        Faulty::new(
            MemoryStore::new(),
            1,
            Faults {
                cas_lost: 1.0,
                ..Faults::none()
            },
        ),
        8,
    );
    let e = s
        .put_conditional(&k("c"), Bytes::from_static(b"x"), Precondition::NotExists)
        .await
        .unwrap_err();
    assert_eq!(e, CasError::Lost);
    assert_eq!(
        s.attempts(),
        0,
        "the CAS path must not go through the retry wrapper"
    );
}

#[tokio::test]
async fn the_limit_never_reaches_zero() {
    // A limit of zero can never be probed again, so the fall would be permanent.
    let s = Congested::new(
        Faulty::new(
            MemoryStore::new(),
            1,
            Faults {
                slow_down: 1.0,
                ..Faults::none()
            },
        ),
        4,
    );
    for _ in 0..10 {
        let _ = s.get(&k("a")).await;
    }
    assert_eq!(s.limit(), 1);
}

#[tokio::test]
async fn capabilities_pass_through_every_decorator() {
    let s = Congested::new(Faulty::new(MemoryStore::new(), 1, Faults::none()), 4);
    assert_eq!(s.capabilities().backend, "memory(Monotonic)");
}

#[tokio::test]
async fn the_report_is_per_probe_not_all_or_nothing() {
    // A backend that fails SOME operations must produce a report naming which -- that is
    // the difference between "this backend is unusable" and "this backend cannot be
    // trusted with durable writes but can serve reads", which is a decision we actually
    // have to make per backend.
    use pstore_blob::Support;
    let s = Faulty::new(
        MemoryStore::new(),
        11,
        Faults {
            cas_contended: 1.0,
            ..Faults::none()
        },
    );
    let r = conformance::run(&s, 200).await;
    assert!(!r.conforms());
    // Reads are untouched, so their probes must still pass.
    for name in ["ranged_read", "suffix_read", "missing_key_is_an_error"] {
        let p = r.probes.iter().find(|p| p.name == name).unwrap();
        assert_eq!(p.outcome, Support::Supported, "{name} should be unaffected");
    }
    assert!(
        r.divergences().len() < r.probes.len(),
        "not everything should fail"
    );
}

#[tokio::test]
async fn a_partially_broken_backend_reports_the_reads_it_can_still_serve() {
    // Writes fail, reads do not. The profile has to say so rather than collapsing to one
    // verdict.
    use pstore_blob::Support;
    let s = Faulty::new(
        MemoryStore::new(),
        13,
        Faults {
            write_error: 1.0,
            ..Faults::none()
        },
    );
    let r = conformance::run(&s, 201).await;
    assert!(!r.conforms());
    assert!(
        r.probes.iter().any(|p| p.outcome == Support::Unsupported),
        "a probe whose setup write failed cannot reach the property it tests"
    );
}
