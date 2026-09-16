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
use std::time::Duration;

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

#[tokio::test]
async fn a_fault_rate_of_one_injects_on_every_call_and_zero_on_none() {
    // ⚠️ The boundaries of the injector's own probability comparisons. Every scenario in
    // the workspace that says "no faults" or "always fails" depends on `r < rate` being
    // exactly that: `<=` makes a rate of 0.0 fire whenever the draw is 0.0, and a test
    // configured for a clean store would inject anyway, occasionally, for reasons no one
    // could find. Mutation testing found nothing distinguished the two.
    let clean = Faulty::new(MemoryStore::new(), 5, Faults::none());
    clean.put(&k("a"), Bytes::from_static(b"x")).await.unwrap();
    for _ in 0..500 {
        clean.get(&k("a")).await.unwrap();
        clean.put(&k("a"), Bytes::from_static(b"x")).await.unwrap();
        clean
            .put_conditional(&k("b"), Bytes::from_static(b"y"), Precondition::NotExists)
            .await
            .ok();
    }

    let always_read = Faulty::new(
        MemoryStore::new(),
        5,
        Faults {
            read_error: 1.0,
            ..Faults::none()
        },
    );
    for _ in 0..200 {
        assert!(always_read.get(&k("a")).await.is_err());
    }

    let always_write = Faulty::new(
        MemoryStore::new(),
        5,
        Faults {
            write_error: 1.0,
            ..Faults::none()
        },
    );
    for _ in 0..200 {
        assert!(
            always_write
                .put(&k("a"), Bytes::from_static(b"x"))
                .await
                .is_err()
        );
    }

    let always_lost = Faulty::new(
        MemoryStore::new(),
        5,
        Faults {
            cas_lost: 1.0,
            ..Faults::none()
        },
    );
    for _ in 0..200 {
        assert!(matches!(
            always_lost
                .put_conditional(&k("a"), Bytes::from_static(b"x"), Precondition::NotExists)
                .await,
            Err(CasError::Lost)
        ));
    }

    let always_contended = Faulty::new(
        MemoryStore::new(),
        5,
        Faults {
            cas_contended: 1.0,
            ..Faults::none()
        },
    );
    for _ in 0..200 {
        assert!(matches!(
            always_contended
                .put_conditional(&k("a"), Bytes::from_static(b"x"), Precondition::NotExists)
                .await,
            Err(CasError::Contended)
        ));
    }
}

#[tokio::test]
async fn injected_faults_land_at_roughly_the_rate_asked_for() {
    // A rate is a contract, not a hint. If the injector's mixer is broken the draws bunch
    // up, a "20%" scenario injects 2% or 90%, and every conclusion drawn from it is about
    // a different experiment than the one described.
    let s = Faulty::new(
        MemoryStore::new(),
        42,
        Faults {
            read_error: 0.25,
            ..Faults::none()
        },
    );
    s.put(&k("a"), Bytes::from_static(b"x")).await.unwrap();
    let n = 4000;
    let failed = {
        let mut c = 0;
        for _ in 0..n {
            if s.get(&k("a")).await.is_err() {
                c += 1;
            }
        }
        c
    };
    assert!(
        (850..=1150).contains(&failed),
        "asked for 25% of {n}, got {failed}"
    );
}

#[tokio::test]
async fn slow_down_first_n_covers_exactly_the_first_n_operations() {
    // An off-by-one here changes "the first attempt fails" into "the first two do", which
    // silently shifts every retry-count assertion built on it.
    let s = Faulty::new(
        MemoryStore::new(),
        1,
        Faults {
            slow_down_first_n: 3,
            ..Faults::none()
        },
    );
    for i in 1..=3 {
        assert!(
            matches!(s.get(&k("a")).await, Err(BlobError::SlowDown)),
            "operation {i} should have been throttled"
        );
    }
    // The fourth is past the window, so it reaches the store and gets an honest 404.
    assert!(matches!(s.get(&k("a")).await, Err(BlobError::NotFound(_))));
}

// ---------------------------------------------------------------------------
// Latency — M0a criterion 4's fourth kind, and M0a.10 carried forward.
//
// ⚠️ `start_paused` is what makes these free. Tokio auto-advances virtual time while the
// runtime is idle, so a 30 ms round trip costs the suite nothing and `elapsed()` still
// reports 30 ms. Latency that made the suite slow would be latency nobody turned on.
// ---------------------------------------------------------------------------

fn with_latency(min_ms: u64, max_ms: u64) -> Faults {
    Faults {
        latency_min: Duration::from_millis(min_ms),
        latency_max: Duration::from_millis(max_ms),
        ..Faults::none()
    }
}

async fn drive(s: &Faulty<MemoryStore>, n: usize) -> Vec<Result<Bytes, BlobError>> {
    let mut out = Vec::new();
    for i in 0..n {
        let k = Key::new(format!("k/{i}"));
        let _ = s.put(&k, Bytes::from_static(b"x")).await;
        out.push(s.get(&k).await);
    }
    out
}

#[tokio::test(start_paused = true)]
async fn latency_is_injected_inside_its_bounds() {
    let s = Faulty::new(MemoryStore::new(), 7, with_latency(10, 30));
    let t = tokio::time::Instant::now();
    let _ = s.get(&Key::new("absent")).await;
    let one = t.elapsed();
    assert!(
        one >= Duration::from_millis(10) && one <= Duration::from_millis(30),
        "a single operation took {one:?}, outside [10ms, 30ms]"
    );

    // Ten operations cost at least ten minimums: the delay is per operation, not per store.
    let t = tokio::time::Instant::now();
    drive(&s, 5).await;
    assert!(
        t.elapsed() >= Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn zero_latency_sleeps_not_at_all() {
    // Not "is fast": exactly zero virtual time, over twenty operations.
    //
    // ⚠️ This does **not** distinguish `sleep(ZERO)` from no sleep at all, and the
    // difference is not observable from outside: a sleep whose deadline has already passed
    // is `Ready` on its first poll. Removing the `is_zero()` guard in `Faulty::delay` is
    // therefore a **provably equivalent mutant**, and it survives — recorded rather than
    // chased. The guard stays because it skips a timer registration per operation on every
    // real runtime in the suite, which is an efficiency argument, not a semantic one.
    let s = Faulty::new(MemoryStore::new(), 7, Faults::none());
    let t = tokio::time::Instant::now();
    drive(&s, 20).await;
    assert_eq!(t.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn the_same_seed_reproduces_the_same_delays() {
    let run = || async {
        let s = Faulty::new(MemoryStore::new(), 99, with_latency(1, 50));
        let mut marks = Vec::new();
        let t = tokio::time::Instant::now();
        for i in 0..8 {
            let _ = s.get(&Key::new(format!("k/{i}"))).await;
            marks.push(t.elapsed());
        }
        marks
    };
    let (a, b) = (run().await, run().await);
    assert_eq!(a, b);
    // And it is actually varying, or "identical" would be satisfied by a constant.
    let gaps: Vec<_> = a.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        gaps.windows(2).any(|w| w[0] != w[1]),
        "delays never varied: {gaps:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn latency_is_independent_of_which_operations_fail() {
    // ⚠️ **The criterion's own word.** M0a criterion 4 asks for 412, 409, 503 and latency
    // "independently". Drawing the delay from the fault stream would make turning latency on
    // silently change *which* operations fail -- and every other test here would still pass,
    // because each one varies only one thing at a time.
    let faults = Faults {
        read_error: 0.3,
        write_error: 0.3,
        cas_lost: 0.2,
        ..Faults::none()
    };
    let quiet = Faulty::new(MemoryStore::new(), 4242, faults);
    let slow = Faulty::new(
        MemoryStore::new(),
        4242,
        Faults {
            latency_min: Duration::from_millis(5),
            latency_max: Duration::from_millis(40),
            ..faults
        },
    );

    let a = drive(&quiet, 30).await;
    let b = drive(&slow, 30).await;
    let shape = |r: &[Result<Bytes, BlobError>]| {
        r.iter()
            .map(|x| x.as_ref().err().map(std::string::ToString::to_string))
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&a), shape(&b), "latency changed the failure sequence");
    assert!(
        shape(&a).iter().any(Option::is_some),
        "the fixture injected no faults at all, so it proves nothing"
    );
}

#[tokio::test(start_paused = true)]
async fn an_inverted_range_delays_by_the_minimum() {
    // max < min is a caller error with no good answer; the useful one is the floor, not a
    // panic in a test helper and not silently no delay at all.
    let s = Faulty::new(MemoryStore::new(), 1, with_latency(25, 5));
    let t = tokio::time::Instant::now();
    let _ = s.get(&Key::new("absent")).await;
    assert_eq!(t.elapsed(), Duration::from_millis(25));
}

// ---------------------------------------------------------------------------
// The generator itself, pinned.
//
// ⚠️ **M0a.11's survivors were all here, and a same-process comparison cannot catch them.**
// `same_seed_reproduces_the_same_failures` runs the same seed twice and compares — which
// *any* deterministic function satisfies, including a badly weakened one. Mutation testing
// said so: `^` for `|`, `>>` for `<<` through SplitMix64's mixing steps all survived.
//
// Determinism from a seed is a **contract, not an implementation detail**: a scenario that
// reproduced a bug last month has to reproduce it today, and on another machine. So the
// sequence is pinned. **If this fails, the fix is never to update the constant** — it is to
// restore the generator, or to accept that every recorded repro seed in the project is void.
// ---------------------------------------------------------------------------

/// Which of `n` reads failed, as a string of `.` and `x`, for a seed and a rate.
async fn failure_shape(seed: u64, rate: f64, n: usize) -> String {
    let s = Faulty::new(
        MemoryStore::new(),
        seed,
        Faults {
            read_error: rate,
            ..Faults::none()
        },
    );
    let k = Key::new("k");
    s.put(&k, Bytes::from_static(b"v")).await.unwrap();
    let mut out = String::new();
    for _ in 0..n {
        out.push(if s.get(&k).await.is_err() { 'x' } else { '.' });
    }
    out
}

#[tokio::test]
async fn the_fault_stream_is_pinned_to_its_seed() {
    assert_eq!(
        failure_shape(12_345, 0.5, 40).await,
        "xxx.xxxx.xx...x...xxxxxxxxxxx...x.xxxx.."
    );
    // A different seed is a different sequence, or "pinned" would be satisfied by a constant.
    assert_ne!(
        failure_shape(12_346, 0.5, 40).await,
        failure_shape(12_345, 0.5, 40).await
    );
}

#[tokio::test(start_paused = true)]
async fn the_latency_stream_is_pinned_to_its_seed() {
    // ⚠️ The second stream needs its own golden values. It is derived from the same seed by
    // a fixed constant, so a change to either the mixer or that constant re-randomises every
    // recorded scenario -- and the fault shape above would not notice, which is the whole
    // reason the two streams are separate.
    let s = Faulty::new(MemoryStore::new(), 12_345, with_latency(0, 100));
    let mut ms = Vec::new();
    let mut last = tokio::time::Instant::now();
    for _ in 0..8 {
        let _ = s.get(&Key::new("absent")).await;
        let now = tokio::time::Instant::now();
        ms.push((now - last).as_millis());
        last = now;
    }
    assert_eq!(ms, vec![84, 1, 58, 90, 50, 83, 32, 22]);
}

#[tokio::test]
async fn a_rate_of_one_fires_on_every_kind() {
    // ⚠️ Aimed at a specific surviving mutant: `r < f.slow_down` on the **write** path
    // mutated to `r == f.slow_down`. Draws are in `[0, 1)`, so `< 1.0` is always true and
    // `== 1.0` is never true -- the store would silently stop injecting slowdowns at the one
    // rate that means "always". `a_fault_rate_of_one_injects_on_every_call_and_zero_on_none`
    // covers the read path only.
    let always = |f: Faults| Faulty::new(MemoryStore::new(), 5, f);
    let k = Key::new("k");

    let s = always(Faults {
        slow_down: 1.0,
        ..Faults::none()
    });
    for _ in 0..8 {
        assert!(matches!(s.get(&k).await, Err(BlobError::SlowDown)));
        assert!(matches!(
            s.put(&k, Bytes::from_static(b"v")).await,
            Err(BlobError::SlowDown)
        ));
    }

    let s = always(Faults {
        write_error: 1.0,
        ..Faults::none()
    });
    for _ in 0..8 {
        assert!(matches!(
            s.put(&k, Bytes::from_static(b"v")).await,
            Err(BlobError::Other(_))
        ));
    }

    for (f, want_lost) in [
        (
            Faults {
                cas_lost: 1.0,
                ..Faults::none()
            },
            true,
        ),
        (
            Faults {
                cas_contended: 1.0,
                ..Faults::none()
            },
            false,
        ),
    ] {
        let s = always(f);
        for _ in 0..8 {
            let e = s
                .put_conditional(&k, Bytes::from_static(b"v"), Precondition::NotExists)
                .await
                .expect_err("a rate of one must always refuse");
            assert_eq!(matches!(e, CasError::Lost), want_lost, "{e:?}");
        }
    }
}

/// ⚠️ **Backlog row 21, and the load-bearing test of M7b.** `get_tag` returned `Option`
/// until this milestone, so `Faulty` forwarded it cleanly while every other read consulted
/// `read_fault`: the one read in the commit loop was the one read faults could not reach.
///
/// Asserted through a **stack** — `Congested` over `Accounted` over `Faulty` — and not on
/// `Faulty` alone, because the way this defect comes back is a decorator swallowing the new
/// error with `.ok().flatten()` and returning the old `None`.
#[tokio::test]
async fn a_refused_probe_is_an_error_and_an_absent_key_is_not() {
    use pstore_blob::Accounted;
    use pstore_types::TenantId;

    let clean_acct = Accounted::new(Faulty::new(MemoryStore::new(), 7, Faults::none()));
    let clean = Congested::new(clean_acct.as_tenant(TenantId(0)), 64);
    // Absence is not an error, at any depth of decorator.
    assert_eq!(clean.get_tag(&k("nothing-here")).await.unwrap(), None);
    clean.put(&k("a"), Bytes::from_static(b"x")).await.unwrap();
    assert!(clean.get_tag(&k("a")).await.unwrap().is_some());

    // ⚠️ Seeded through a store whose faults are off, then probed through one whose are on:
    // at `read_error` 1.0 a `put` would fail too, and a test that cannot seed is measuring
    // its own fixture.
    let inner = MemoryStore::new();
    inner.put(&k("a"), Bytes::from_static(b"x")).await.unwrap();
    let acct = Accounted::new(Faulty::new(
        inner,
        7,
        Faults {
            read_error: 1.0,
            ..Faults::none()
        },
    ));
    let refusing = Congested::new(acct.as_tenant(TenantId(0)), 64);
    let err = refusing
        .get_tag(&k("a"))
        .await
        .expect_err("a probe through a store refusing every read reported an answer");
    assert!(
        matches!(err, BlobError::Other(_)),
        "expected the injected read fault, got {err:?}"
    );
    // And the tenant was still billed for the request it made: a probe that failed is a
    // probe that happened.
    assert_eq!(
        acct.count(TenantId(0), pstore_blob::OpClass::Read),
        1,
        "a refused probe was not billed as the read it issued"
    );
}

/// ⚠️ **Backlog row 23, opened by M7b's own review.** The probe was the only read that did
/// not go through `with_retry`, and until `get_tag` became fallible that was forced: there
/// was no error to retry. Now a 503 on the **commit loop's only read** would abandon the
/// rebase instead of backing off — and the AIMD limit would never learn about the throttle
/// either, because `on_slow_down` is reached only from the retry loop.
///
/// The direct forward is a *silent* defect: the probe still answers. So this pins the two
/// things that differ — the retry, and the cut.
#[tokio::test]
async fn a_throttled_probe_is_retried_like_every_other_read() {
    let inner = MemoryStore::new();
    inner.put(&k("a"), Bytes::from_static(b"x")).await.unwrap();
    let flaky = Faulty::new(
        inner,
        1,
        Faults {
            slow_down_first_n: 1,
            ..Faults::none()
        },
    );
    let s = Congested::new(flaky, 32);
    assert!(
        s.get_tag(&k("a")).await.unwrap().is_some(),
        "a transient 503 on the probe was not retried"
    );
    assert_eq!(s.attempts(), 2, "exactly one retry, like `get`");
    // ⚠️ And the throttle reached the controller. A retry that does not cut the limit
    // answers this test's first assertion and leaves the client hammering a shedding
    // prefix at full concurrency.
    assert_eq!(
        s.limit(),
        16,
        "one 503 on a probe must halve 32, not floor it"
    );

    // A permanent 503 must still terminate: the retry is bounded, or a transient stressor
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
        s.get_tag(&k("a")).await.unwrap_err(),
        BlobError::SlowDown
    ));
    // ⚠️ The exact count, not a range. `<= 5` -- which the older `retries_are_bounded`
    // still uses -- leaves a slot of slack above the real bound, so raising
    // `MAX_ATTEMPTS` from 4 to 5 would grow the retry budget 25% with the suite green.
    assert_eq!(s.attempts(), 4, "the bound is MAX_ATTEMPTS, exactly");
}
