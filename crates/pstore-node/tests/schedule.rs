//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

//! When a node heals, reports and counts (M31): the loop's decisions, out of `main`.
//!
//! ⚠️ The heal cadence counted polls and was compared with a count of views, so at the 200 ms
//! poll a fleet measures at a node healed every 50 s instead of every `HEAL_PERIOD`. It lived
//! in `main`, where no gate could see it, under a comment saying it had been fixed.

use pstore_node::policy::jitter;
use pstore_node::schedule::{Cadence, Clock, Tick, chitchat, owns_period, periods, probe_loss};
use pstore_node::{DEFAULT_GOSSIP_PERIOD, HEAL_PERIOD};
use std::time::Duration;

const ID: &str = "node-31";
const MS: fn(u64) -> Duration = Duration::from_millis;

#[test]
fn heals_at_the_heal_period_at_its_slot() {
    for (poll, views) in [(MS(200), 10), (MS(1000), 10), (MS(2000), 5)] {
        let c = Cadence::new(poll, 5, ID);
        assert_eq!(c.heal_every, views, "poll {poll:?}");
        // A view is `view_every` polls, so a heal is HEAL_PERIOD of wall time.
        assert_eq!(
            Duration::from_millis(poll.as_millis() as u64 * c.view_every * c.heal_every),
            HEAL_PERIOD,
            "poll {poll:?}"
        );
        let mut clock = Clock::new(c);
        let mut heals = Vec::new();
        for _ in 0..(c.view_every * views * 6) {
            let t = clock.tick(3);
            if t.heal {
                heals.push(t.view.expect("a heal off a view poll"));
            }
        }
        let slot = jitter(ID, views);
        assert_eq!(heals.len(), 6, "poll {poll:?}: {heals:?}");
        assert!(
            heals.iter().all(|t| t % views == slot),
            "poll {poll:?}: {heals:?}"
        );
    }
    // Slower than the heal period: every view heals.
    let c = Cadence::new(HEAL_PERIOD * 3, 5, ID);
    assert_eq!((c.view_every, c.heal_every), (1, 1));
}

#[test]
fn ownership_is_reported_in_seconds() {
    for (poll, views) in [(MS(200), 5), (MS(1000), 5), (MS(2000), 2)] {
        let c = Cadence::new(poll, 5, ID);
        assert_eq!(c.owns_every, views, "poll {poll:?}");
        let mut clock = Clock::new(c);
        let owned: Vec<u64> = (0..c.view_every * 20)
            .map(|_| clock.tick(3))
            .filter(|t| t.owns)
            .map(|t| t.view.unwrap())
            .collect();
        let want: Vec<u64> = (1..=20).filter(|t| t % views == 0).collect();
        assert_eq!(owned, want, "poll {poll:?}");
    }
    let mut never = Clock::new(Cadence::new(MS(1000), 0, ID));
    assert!(
        (0..100).all(|_| !never.tick(3).owns),
        "a zero period reported"
    );
    assert_eq!(owns_period(None), 5);
    assert_eq!(owns_period(Some("x")), 5);
    assert_eq!(owns_period(Some("0")), 0);
    assert_eq!(owns_period(Some("7")), 7);
}

#[test]
fn one_poll_runs_in_mains_order() {
    let c = Cadence::new(MS(200), 5, ID);
    assert_eq!(c.view_every, 5);
    let mut clock = Clock::new(c);
    let sizes = [3, 3, 4, 4, 4, 4, 4, 2, 2, 2];
    let got: Vec<Tick> = sizes.iter().map(|s| clock.tick(*s)).collect();
    // A change on the first poll, and on each move only.
    let changes: Vec<(usize, usize)> = got
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.changed.map(|s| (i, s)))
        .collect();
    assert_eq!(changes, [(0, 3), (2, 4), (7, 2)]);
    // A view on every fifth poll, numbered from 1; nothing else off a view poll.
    let views: Vec<(usize, u64)> = got
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.view.map(|v| (i, v)))
        .collect();
    assert_eq!(views, [(4, 1), (9, 2)]);
    for (i, t) in got.iter().enumerate() {
        if t.view.is_none() {
            assert!(!t.heal && !t.owns, "poll {i} acted off a view");
        }
    }
    // The count moves before the checks: the first view is checked at 1, never at 0.
    let first = (0..10_000)
        .map(|i| format!("n{i}"))
        .find(|id| jitter(id, 10) == 1)
        .unwrap();
    let mut clock = Clock::new(Cadence::new(MS(1000), 1, &first));
    let t = clock.tick(1);
    assert_eq!((t.view, t.heal, t.owns), (Some(1), true, true));
    // A size of `usize::MAX` is never a change, as `main`'s sentinel made it.
    let mut clock = Clock::new(Cadence::new(MS(1000), 5, ID));
    assert_eq!(clock.tick(usize::MAX).changed, None);
}

#[test]
fn settings_fall_back_as_main_did() {
    let d = DEFAULT_GOSSIP_PERIOD;
    assert_eq!(periods(None, None), (d, d));
    assert_eq!(periods(Some("x"), None), (d, d));
    assert_eq!(periods(Some("0"), None), (d, d));
    assert_eq!(periods(Some("250"), None), (MS(250), MS(250)));
    assert_eq!(periods(Some("250"), Some("x")), (MS(250), MS(250)));
    assert_eq!(periods(Some("250"), Some("0")), (MS(250), MS(250)));
    assert_eq!(periods(Some("250"), Some("100")), (MS(250), MS(100)));
    assert_eq!(periods(None, Some("100")), (d, MS(100)));
    assert!(probe_loss(None).abs() < f64::EPSILON);
    assert!(probe_loss(Some("x")).abs() < f64::EPSILON);
    assert!((probe_loss(Some("0.25")) - 0.25).abs() < f64::EPSILON);
    assert!(chitchat(Some("chitchat")));
    for other in [None, Some("swim"), Some(""), Some("Chitchat")] {
        assert!(!chitchat(other), "{other:?}");
    }
}
