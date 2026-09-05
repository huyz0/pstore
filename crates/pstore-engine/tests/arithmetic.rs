//! Two small functions whose behaviour nothing else can observe.
//!
//! Both were found by mutation testing, and both for the same reason: their output is
//! consumed by something that tolerates a wide range of values. The backoff feeds a
//! `sleep`, so any duration "works". The nonce feeds a byte buffer, so any number
//! "works". A test that exercises them through the commit path therefore proves nothing
//! about them at all — the mutants that inverted the shift, swapped the arithmetic, and
//! replaced XOR with OR all passed the entire suite.
//!
//! What makes them testable is that each has a *contract* separate from its use.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_engine::{backoff_delay, nonce_for};
use pstore_types::{Epoch, LaneId};
use std::collections::BTreeSet;

#[test]
fn backoff_grows_with_the_attempt() {
    // The point of backing off: a writer that has lost repeatedly waits longer, so a
    // contended tenant spreads out instead of hammering. An inverted shift makes the
    // opposite happen and no functional test can tell.
    let lane = LaneId(0);
    for a in 0..6u32 {
        assert!(
            backoff_delay(lane, a + 1) > backoff_delay(lane, a),
            "attempt {} did not wait longer than {a}",
            a + 1
        );
    }
    assert_eq!(backoff_delay(lane, 0), std::time::Duration::from_micros(1));
}

#[test]
fn backoff_is_capped() {
    // Without a cap the 24th attempt waits 2^24 microseconds -- 17 seconds -- to retry an
    // operation that takes milliseconds.
    let lane = LaneId(0);
    let capped = backoff_delay(lane, 6);
    for a in 6..64u32 {
        assert_eq!(
            backoff_delay(lane, a),
            capped,
            "attempt {a} escaped the cap"
        );
    }
    assert!(capped < std::time::Duration::from_millis(1));
}

#[test]
fn lanes_do_not_back_off_in_lockstep() {
    // ⚠️ The livelock guard. If every loser waits the same time, they all wake together,
    // collide with the same peers, and lose again -- which is how this surfaced in M1, as
    // a flaky test rather than a failing one. A jitter of zero for any lane puts that lane
    // back in lockstep with every other zero.
    for attempt in 0..8u32 {
        let delays: BTreeSet<_> = (0..8u64)
            .map(|l| backoff_delay(LaneId(l), attempt))
            .collect();
        assert_eq!(
            delays.len(),
            8,
            "attempt {attempt}: 8 lanes produced only {} distinct delays",
            delays.len()
        );
        assert!(
            !delays.contains(&std::time::Duration::ZERO),
            "attempt {attempt}: some lane does not wait at all"
        );
    }
}

#[test]
fn nonces_never_collide_across_epochs_and_lanes() {
    // ⚠️ The ABA guard's actual requirement, and the one thing every commit-path test is
    // blind to: two DIFFERENT commits must not produce the same nonce. `^` satisfies this;
    // `|` and `&` do not, and both pass every other test in the workspace.
    let mut seen = std::collections::BTreeMap::new();
    for e in 1..64u64 {
        for l in 0..64u64 {
            let n = nonce_for(Epoch(e), LaneId(l));
            if let Some(prev) = seen.insert(n, (e, l)) {
                panic!("nonce {n:#x} produced by both {prev:?} and {:?}", (e, l));
            }
        }
    }
}

#[test]
fn a_nonce_changes_when_only_the_epoch_does() {
    // The single-writer case, which is the common one: consecutive commits from the same
    // lane must not encode to the same bytes, or a content-derived ETag repeats.
    let lane = LaneId(3);
    let a: BTreeSet<u64> = (1..500).map(|e| nonce_for(Epoch(e), lane)).collect();
    assert_eq!(a.len(), 499, "consecutive epochs reused a nonce");
}
