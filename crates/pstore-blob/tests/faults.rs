//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The fault-injecting store is the primary correctness vehicle (D-3, D-99): no emulator
//! implements our CAS primitive faithfully, so the only backend whose behaviour we can
//! both control and assert is one we wrote.
use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, CasError, Congested, Faults, Faulty, Key, MemoryStore, Precondition,
};

fn k(s: &str) -> Key {
    Key::new(s)
}

#[tokio::test]
async fn same_seed_reproduces_the_same_failures() {
    // A failing test must replay exactly, or a rare interleaving is unfixable.
    async fn trace(seed: u64) -> Vec<bool> {
        let s = Faulty::new(
            MemoryStore::new(),
            seed,
            Faults {
                read_error: 0.5,
                ..Faults::none()
            },
        );
        s.put(&k("a"), Bytes::from_static(b"x")).await.unwrap();
        let mut out = Vec::new();
        for _ in 0..40 {
            out.push(s.get(&k("a")).await.is_ok());
        }
        out
    }
    let a = trace(0xDEAD_BEEF).await;
    assert_eq!(
        a,
        trace(0xDEAD_BEEF).await,
        "same seed must replay identically"
    );
    assert_ne!(a, trace(0x1234_5678).await, "a different seed must diverge");
    assert!(
        a.iter().any(|ok| *ok) && a.iter().any(|ok| !ok),
        "0.5 should mix outcomes"
    );
}

#[tokio::test]
async fn a_zero_rate_injects_nothing() {
    // The false-refusal path: with faults off the decorator must be transparent.
    let s = Faulty::new(MemoryStore::new(), 1, Faults::none());
    s.put(&k("a"), Bytes::from_static(b"x")).await.unwrap();
    for _ in 0..100 {
        assert!(s.get(&k("a")).await.is_ok());
    }
}

#[tokio::test]
async fn each_fault_kind_is_injected_independently() {
    let slow = Faulty::new(
        MemoryStore::new(),
        1,
        Faults {
            slow_down: 1.0,
            ..Faults::none()
        },
    );
    assert!(matches!(
        slow.get(&k("a")).await.unwrap_err(),
        BlobError::SlowDown
    ));

    let lost = Faulty::new(
        MemoryStore::new(),
        1,
        Faults {
            cas_lost: 1.0,
            ..Faults::none()
        },
    );
    assert_eq!(
        lost.put_conditional(&k("a"), Bytes::from_static(b"x"), Precondition::NotExists)
            .await
            .unwrap_err(),
        CasError::Lost
    );

    // 409 is the one no emulator emits, so it is testable nowhere else.
    let cont = Faulty::new(
        MemoryStore::new(),
        1,
        Faults {
            cas_contended: 1.0,
            ..Faults::none()
        },
    );
    assert_eq!(
        cont.put_conditional(&k("a"), Bytes::from_static(b"x"), Precondition::NotExists)
            .await
            .unwrap_err(),
        CasError::Contended
    );
}

#[tokio::test]
async fn an_injected_write_fault_does_not_reach_the_backend() {
    // A fault that fails AFTER mutating would make the store diverge from what the
    // caller was told, and every later assertion would be against a fiction.
    let inner = MemoryStore::new();
    let s = Faulty::new(
        inner.clone(),
        1,
        Faults {
            cas_lost: 1.0,
            ..Faults::none()
        },
    );
    let _ = s
        .put_conditional(&k("a"), Bytes::from_static(b"x"), Precondition::NotExists)
        .await;
    assert!(
        inner.get(&k("a")).await.is_err(),
        "the refused write must not have landed"
    );
}

#[tokio::test]
async fn slowdown_reduces_concurrency_then_recovers() {
    // 503 SlowDown is a normal signal, not an error. Not backing off produces a 503
    // storm; not recovering leaves throughput permanently halved after one blip.
    let inner = MemoryStore::new();
    inner.put(&k("a"), Bytes::from_static(b"x")).await.unwrap();
    let flaky = Faulty::new(
        inner,
        7,
        Faults {
            slow_down: 1.0,
            ..Faults::none()
        },
    );
    let s = Congested::new(flaky.clone(), 32);
    assert_eq!(s.limit(), 32);

    let _ = s.get(&k("a")).await;
    let after = s.limit();
    assert!(after < 32, "a 503 must reduce the limit, got {after}");

    flaky.set_faults(Faults::none());
    for _ in 0..200 {
        s.get(&k("a")).await.unwrap();
    }
    assert!(
        s.limit() > after,
        "success must recover the limit, still {}",
        s.limit()
    );
    assert!(s.limit() <= 32, "recovery must not exceed the ceiling");
}

#[tokio::test]
async fn retries_are_bounded() {
    // A persistent 503 must terminate. Retrying forever is how a transient stressor
    // becomes a metastable failure.
    let always = Faulty::new(
        MemoryStore::new(),
        1,
        Faults {
            slow_down: 1.0,
            ..Faults::none()
        },
    );
    let s = Congested::new(always, 8);
    assert!(matches!(
        s.get(&k("a")).await.unwrap_err(),
        BlobError::SlowDown
    ));
    assert!(s.attempts() <= 5, "attempted {} times", s.attempts());
    assert!(s.attempts() >= 2, "it should have retried at least once");
}

#[tokio::test]
async fn a_transient_slowdown_is_retried_and_succeeds() {
    let inner = MemoryStore::new();
    inner.put(&k("a"), Bytes::from_static(b"ok")).await.unwrap();
    // Fails the first attempt only.
    let flaky = Faulty::new(
        inner,
        1,
        Faults {
            slow_down_first_n: 1,
            ..Faults::none()
        },
    );
    let s = Congested::new(flaky, 8);
    assert_eq!(&s.get(&k("a")).await.unwrap()[..], b"ok");
    assert_eq!(s.attempts(), 2);
}

#[tokio::test]
async fn a_slow_down_halves_the_limit_rather_than_merely_lowering_it() {
    // ⚠️ Direction is not enough. `limit / 2` and `limit % 2` both make the limit smaller,
    // and only one of them is multiplicative decrease -- the other collapses straight to
    // the floor on the first 503 and then crawls back one at a time. Mutation testing
    // found the existing "it went down" assertion could not tell them apart.
    let inner = MemoryStore::new();
    inner.put(&k("a"), Bytes::from_static(b"ok")).await.unwrap();
    let flaky = Faulty::new(
        inner,
        3,
        Faults {
            slow_down_first_n: 1,
            ..Faults::none()
        },
    );
    let s = Congested::new(flaky, 32);
    let _ = s.get(&k("a")).await;
    assert_eq!(s.limit(), 16, "one 503 must halve 32, not floor it");
}

#[tokio::test]
async fn recovery_is_additive_and_slow() {
    // Additive increase is the other half of AIMD, and "slow" is the point: recovering a
    // whole step per success would undo a cut with one lucky request, which is how a
    // throttled client oscillates instead of settling.
    let inner = MemoryStore::new();
    inner.put(&k("a"), Bytes::from_static(b"ok")).await.unwrap();
    let s = Congested::new(inner, 32);

    // The limit starts at the ceiling, so drive it down first and measure from there.
    for _ in 0..8 {
        s.get(&k("a")).await.unwrap();
    }
    assert_eq!(s.limit(), 32, "already at the ceiling, nothing to add");

    let low = Congested::new(MemoryStore::new(), 1000);
    let base = low.limit();
    for _ in 0..80 {
        let _ = low.get(&k("absent")).await;
    }
    assert_eq!(
        low.limit(),
        base,
        "a failed request must not count as a success"
    );
}

#[test]
fn the_retry_delay_is_exponential_and_capped() {
    // An inverted shift turns backoff into no backoff, and the retry loop cannot tell:
    // its only observable is how long it slept.
    for a in 0..10u32 {
        assert!(
            pstore_blob::retry_delay(a + 1) > pstore_blob::retry_delay(a),
            "attempt {} did not wait longer than {a}",
            a + 1
        );
    }
    assert_eq!(
        pstore_blob::retry_delay(0),
        std::time::Duration::from_millis(1)
    );
    // Capped, so a pathological attempt count cannot wait for days.
    assert_eq!(pstore_blob::retry_delay(16), pstore_blob::retry_delay(99));
}
