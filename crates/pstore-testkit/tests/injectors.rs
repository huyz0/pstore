//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The fault injectors, tested as the load-bearing components they are.
//!
//! `Flaky` is what makes the OQ-91 proof a proof: if it silently stopped refusing writes,
//! the scenario would run 64 seeds of a perfectly healthy store and report success. `Gated`
//! is what makes the compaction race a race rather than a lottery. Neither has any user
//! but tests, which is exactly why neither can be left untested — a harness that quietly
//! stops working takes every result built on it with it, and reports green while doing so.

use bytes::Bytes;
use pstore_blob::{BlobError, BlobStore, CasError, Key, Precondition};
use pstore_testkit::{flaky::Flaky, gated::Gated};
use std::sync::Arc;

fn k(s: &str) -> Key {
    Key::new(s)
}

#[tokio::test]
async fn refusing_refuses_exactly_the_named_writes() {
    // The pinned regression in `oq91.rs` depends on ordinal 2 and no other being refused.
    // An off-by-one here moves the hole somewhere else and the test still passes, having
    // exercised a different scenario than the one it documents.
    let s = Flaky::refusing(&[1, 3]);
    let mut outcomes = Vec::new();
    for i in 0..5 {
        outcomes.push(
            s.put(&k(&format!("k{i}")), Bytes::from_static(b"x"))
                .await
                .is_ok(),
        );
    }
    assert_eq!(
        outcomes,
        vec![true, false, true, false, true],
        "wrong writes refused"
    );
    assert_eq!(s.failures(), 2);
    // And the refused writes did not land: a refusal that still writes would make the
    // "hole in the lane" regression untestable.
    assert!(s.get(&k("k1")).await.is_err());
    assert!(s.get(&k("k0")).await.is_ok());
}

#[tokio::test]
async fn a_refused_write_never_reaches_the_store() {
    let s = Flaky::refusing(&[0]);
    assert!(s.put(&k("a"), Bytes::from_static(b"v")).await.is_err());
    assert!(
        s.get(&k("a")).await.is_err(),
        "the refusal half-landed, which is a different fault than the one asked for"
    );
}

#[tokio::test]
async fn a_rate_is_honoured_at_both_extremes_and_in_between() {
    // Zero must mean zero. Every "healthy store" scenario in the workspace rests on it.
    let clean = Flaky::new(1, 0.0);
    for i in 0..300 {
        clean
            .put(&k(&format!("k{i}")), Bytes::from_static(b"x"))
            .await
            .unwrap();
    }
    assert_eq!(clean.failures(), 0);

    let doomed = Flaky::new(1, 1.0);
    for _ in 0..100 {
        assert!(doomed.put(&k("a"), Bytes::from_static(b"x")).await.is_err());
    }

    // And a middling rate lands near what was asked for, or the OQ-91 scenario's "15% of
    // writes refused" describes an experiment that did not happen.
    let some = Flaky::new(7, 0.3);
    let n = 4000;
    for i in 0..n {
        let _ = some
            .put(&k(&format!("k{i}")), Bytes::from_static(b"x"))
            .await;
    }
    let f = some.failures();
    assert!((1000..=1400).contains(&f), "asked for 30% of {n}, got {f}");
}

#[tokio::test]
async fn the_same_seed_refuses_the_same_writes() {
    // Without this a failing OQ-91 seed cannot be replayed, which is the whole reason the
    // scenario is seeded rather than random.
    let run = |seed: u64| async move {
        let s = Flaky::new(seed, 0.4);
        let mut out = Vec::new();
        for i in 0..200 {
            out.push(
                s.put(&k(&format!("k{i}")), Bytes::from_static(b"x"))
                    .await
                    .is_ok(),
            );
        }
        out
    };
    assert_eq!(run(9).await, run(9).await, "the same seed diverged");
    assert_ne!(run(9).await, run(10).await, "different seeds agreed");
}

#[tokio::test]
async fn refusing_reads_refuses_every_kind_of_read() {
    // ⚠️ `head` included. It is how the WAL tail is probed, so an injector that left it
    // working would model a backend that does not exist -- and would hide the engine's
    // most important decision, whether a missing key is a gap or an outage.
    let s = Flaky::refusing_reads();
    assert!(s.get(&k("a")).await.is_err());
    assert!(s.get_range(&k("a"), 0..4).await.is_err());
    assert!(s.get_suffix(&k("a"), 4).await.is_err());
    assert!(s.get_with_tag(&k("a")).await.is_err());
    assert!(s.head(&k("a")).await.is_err());
    // Writes still work: this models a backend that cannot serve reads, not one that is
    // gone, and conflating the two would make the scenario untargetable.
    assert!(s.put(&k("a"), Bytes::from_static(b"x")).await.is_ok());
}

#[tokio::test]
async fn always_contended_reports_contention_and_not_a_lost_race() {
    // The distinction is the whole retry protocol: `Lost` means rebase because someone
    // else won, `Contended` means the backend could not evaluate the condition and the
    // same attempt should be retried unchanged.
    let s = Flaky::always_contended();
    for _ in 0..20 {
        assert!(matches!(
            s.put_conditional(&k("a"), Bytes::from_static(b"x"), Precondition::NotExists)
                .await,
            Err(CasError::Contended)
        ));
    }
    assert!(s.put(&k("a"), Bytes::from_static(b"x")).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_armed_gate_releases_its_writers_together() {
    // If the gate let writers through one at a time, the compaction race would be
    // sequential and `concurrent_compactors_produce_one_winner` would pass by describing
    // something that never happened.
    let s = Arc::new(Gated::new(3));
    s.arm();
    let mut tasks = Vec::new();
    for i in 0..3u64 {
        let s = Arc::clone(&s);
        tasks.push(tokio::spawn(async move {
            s.put_conditional(
                &Key::new(format!("k{i}")),
                Bytes::from_static(b"x"),
                Precondition::NotExists,
            )
            .await
            .is_ok()
        }));
    }
    // The barrier only releases once all three arrive, so this completing at all is the
    // assertion: two arrivals would hang here forever.
    for t in tasks {
        assert!(t.await.unwrap());
    }
    assert!(
        s.raced(),
        "the barrier gave up instead of releasing together"
    );
}

#[tokio::test]
async fn an_unarmed_gate_does_not_block() {
    // Scenario setup happens before the race. A gate that counted those writes would
    // release the barrier before the racers ever arrived -- or deadlock the setup.
    let s = Gated::new(4);
    for i in 0..10u64 {
        s.put_conditional(
            &Key::new(format!("k{i}")),
            Bytes::from_static(b"x"),
            Precondition::NotExists,
        )
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gate_does_not_hold_writers_after_its_quota() {
    // A loser that rebases and retries must not re-enter the barrier: it would wait for
    // peers that have already finished and deadlock the test rather than fail it, which
    // is the harder failure to read.
    let s = Arc::new(Gated::new(2));
    s.arm();
    let a = {
        let s = Arc::clone(&s);
        tokio::spawn(async move { s.put(&k("x"), Bytes::from_static(b"1")).await.is_ok() })
    };
    let mut gated = Vec::new();
    for i in 0..2u64 {
        let s = Arc::clone(&s);
        gated.push(tokio::spawn(async move {
            s.put_conditional(
                &Key::new(format!("g{i}")),
                Bytes::from_static(b"x"),
                Precondition::NotExists,
            )
            .await
            .is_ok()
        }));
    }
    for t in gated {
        assert!(t.await.unwrap());
    }
    assert!(a.await.unwrap());
    // Past the quota, and this must simply return.
    s.put_conditional(
        &k("after"),
        Bytes::from_static(b"x"),
        Precondition::NotExists,
    )
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_lone_writer_at_an_armed_gate_waits_it_out_and_is_not_a_race() {
    // The gate's failure mode, which no other test reaches: its racers never come. The
    // first `n` writers are HELD -- so one of two waits the whole timeout (the paused clock
    // advances it) -- and `raced()` must then say so. Without this, a gate that let the
    // first `n` straight through, or a `raced()` that always answered yes, passes every
    // test above: "one winner" is equally satisfied by a race and by a writer alone.
    let s = Gated::new(2);
    s.arm();
    let started = tokio::time::Instant::now();
    s.put_conditional(
        &k("alone"),
        Bytes::from_static(b"x"),
        Precondition::NotExists,
    )
    .await
    .unwrap();
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(5),
        "a lone writer was not held at the barrier ({:?})",
        started.elapsed()
    );
    assert!(
        !s.raced(),
        "a writer that raced nobody was reported as a race"
    );
}

#[tokio::test]
async fn claims_hands_back_the_objects_it_holds() {
    // `store()` exists so a differently-profiled `Claims` can re-probe the SAME objects. A
    // fresh `MemoryStore` would be a re-probe of an empty world.
    use pstore_testkit::claims::Claims;
    let c = Claims::conforming();
    c.put(&k("kept"), Bytes::from_static(b"v")).await.unwrap();
    assert_eq!(
        c.store().get(&k("kept")).await.unwrap(),
        Bytes::from_static(b"v")
    );
}

#[tokio::test]
async fn an_injected_error_says_it_was_injected() {
    // A test that fails on an injected fault should say so in its own output rather than
    // sending the reader looking for a real bug.
    let s = Flaky::refusing(&[0]);
    let e = s.put(&k("a"), Bytes::from_static(b"x")).await.unwrap_err();
    assert!(
        matches!(&e, BlobError::Other(m) if m.contains("injected")),
        "an injected failure surfaced as {e}"
    );
}
