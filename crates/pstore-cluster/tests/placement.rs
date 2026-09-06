//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Placement (D-5), and the property the whole scaling story rests on.
//!
//! ⚠️ **Churn is large and data movement is zero.** `routing-and-placement.md`: growing
//! 1,000 → 2,000 nodes remaps "~50% of keys… **Nothing is copied.**" Nodes own nothing, so a
//! fleet change moves no bytes between machines and only changes which node *caches* what.
//! An earlier draft of this milestone's spec conflated the two and asserted a churn bound
//! **below the arithmetic floor** — 3.6% where the minimum is 33.3%.

use pstore_cluster::{Placement, Roster};
use std::collections::{BTreeMap, BTreeSet};

const R: usize = 3;
const KEYS: usize = 20_000;

fn nodes(n: usize) -> Roster {
    Roster::from_nodes((0..n).map(|i| format!("n{i}")))
}

const BIG_KEYS: usize = 100_000;

fn big_keys() -> Vec<String> {
    (0..BIG_KEYS)
        .map(|i| format!("idx{i}/s{}", i % 7))
        .collect()
}

fn keys() -> Vec<String> {
    (0..KEYS).map(|i| format!("idx{i}/s{}", i % 7)).collect()
}

/// The share of replica slots that must move when a fleet goes from `a` to `b` nodes: the
/// changed nodes' own share, which no scheme can beat.
fn floor(a: usize, b: usize) -> f64 {
    a.abs_diff(b) as f64 / a.max(b) as f64
}

#[test]
fn placement_is_balanced_at_c_thirty_two() {
    // ⚠️ 1.25x, measured at 1.205 — NOT the "within 10–15% of average" that
    // `routing-and-placement.md` credits LRH with. At C=32 against N=100 the window is a
    // third of the fleet; measured 1.171 at C=64 and 1.360 at C=20, so the corpus figure
    // appears to hold only where C is small relative to N. The pinned number is the measured
    // one, and the discrepancy is a correction rather than a tolerance.
    let r = nodes(100);
    let p = Placement::new(&r);
    // ⚠️ 100,000 keys, matching the sample the pinned 1.205 was measured over. LRH's
    // imbalance is structural — a node covering a wide ring gap starts more windows — but
    // at 20,000 keys statistical noise is still visible on top of it, and comparing a
    // measurement to a threshold taken at a different sample size compares two things.
    let mut load: BTreeMap<&str, usize> = BTreeMap::new();
    for k in big_keys() {
        for n in p.place(&k, R) {
            *load.entry(n).or_default() += 1;
        }
    }
    let mean = (BIG_KEYS * R) as f64 / 100.0;
    let max = *load.values().max().unwrap() as f64;
    assert!(
        max / mean <= 1.25,
        "hottest node holds {:.3}x the mean",
        max / mean
    );
    assert_eq!(load.len(), 100, "some node received nothing at all");
}

#[test]
fn a_single_node_addition_moves_almost_nothing() {
    // ⚠️ Bounded in ABSOLUTE slots, not as a ratio to the minimum. The floor for one node
    // added to 100 is 1%, so a ratio magnifies ring-position luck: across 20 different
    // added-node ids the ratio spans 1.47x to 2.10x, and across five strong hash functions
    // 1.38x to 1.95x. The spec first pinned 1.6x from a SINGLE sample of that statistic,
    // which is not a pinned threshold. The absolute figure is ~1.9% and stable.
    let base: Vec<String> = (0..100).map(|i| format!("n{i}")).collect();
    let ra = Roster::from_nodes(base.clone());
    let pa = Placement::new(&ra);
    let ks = keys();
    for trial in 0..8 {
        let rb = Roster::from_nodes(base.iter().cloned().chain([format!("extra{trial}")]));
        let pb = Placement::new(&rb);
        let moved: usize = ks
            .iter()
            .map(|k| {
                let before: BTreeSet<_> = pa.place(k, R).into_iter().collect();
                let after: BTreeSet<_> = pb.place(k, R).into_iter().collect();
                before.difference(&after).count()
            })
            .sum();
        let frac = moved as f64 / (KEYS * R) as f64;
        assert!(
            frac <= 0.03,
            "trial {trial}: adding one node to 100 moved {:.2}% of slots",
            frac * 100.0
        );
    }
}

#[test]
fn a_bulk_fleet_change_moves_no_more_than_the_minimum() {
    // The operationally interesting case, and the stable one: 1.28-1.36x across five
    // independent hashes, against a floor of 33%.
    for (a, b) in [(100usize, 150usize), (150, 100)] {
        let (ra, rb) = (nodes(a), nodes(b));
        let (pa, pb) = (Placement::new(&ra), Placement::new(&rb));
        let moved: usize = keys()
            .iter()
            .map(|k| {
                let before: BTreeSet<_> = pa.place(k, R).into_iter().collect();
                let after: BTreeSet<_> = pb.place(k, R).into_iter().collect();
                before.difference(&after).count()
            })
            .sum();
        let frac = moved as f64 / (KEYS * R) as f64;
        let min = floor(a, b);
        assert!(
            frac <= min * 1.5,
            "{a} -> {b}: moved {:.1}% of slots against a floor of {:.1}% ({:.2}x)",
            frac * 100.0,
            min * 100.0,
            frac / min
        );
    }
}

#[test]
fn growth_moves_no_less_than_the_new_nodes_share() {
    // ⚠️ The other side, and the one that matters. "Moves at most X" is satisfied by **0%**,
    // which is exactly what a node that reads the roster and never rebuilds its ring would
    // score — the most likely bug, scoring perfect. The added nodes must actually receive
    // work.
    let (ra, rb) = (nodes(100), nodes(150));
    let (pa, pb) = (Placement::new(&ra), Placement::new(&rb));
    let new: BTreeSet<String> = (100..150).map(|i| format!("n{i}")).collect();
    let mut to_new = 0usize;
    for k in keys() {
        to_new += pb.place(&k, R).iter().filter(|n| new.contains(**n)).count();
        let _ = pa.place(&k, R);
    }
    let share = to_new as f64 / (KEYS * R) as f64;
    let expected = 50.0 / 150.0;
    assert!(
        share >= expected * 0.8,
        "the 50 new nodes received {:.1}% of placements, under 0.8x their {:.1}% share: the \
         ring is not being rebuilt",
        share * 100.0,
        expected * 100.0
    );
}

#[test]
fn placement_is_deterministic_across_rosters_built_differently() {
    // Order of construction must not matter: a roster is a set, and two nodes that learned
    // membership in different orders must place identically or they disagree about who
    // serves what.
    let forward = Roster::from_nodes((0..50).map(|i| format!("n{i}")));
    let backward = Roster::from_nodes((0..50).rev().map(|i| format!("n{i}")));
    let (a, b) = (Placement::new(&forward), Placement::new(&backward));
    for k in keys().iter().take(2_000) {
        assert_eq!(a.place(k, R), b.place(k, R), "key {k}");
    }
}

#[test]
fn an_overloaded_node_sheds_only_its_own_shards() {
    // Skipping must step past one node, not reshuffle the ring: shedding load from a hot
    // node by moving everyone else's placements would turn a local problem into a fleet-wide
    // cache flush.
    let r = nodes(100);
    let plain = Placement::new(&r);
    let hot = "n7";
    let skipped = Placement::new(&r).with_overloaded([hot]);

    let (mut changed, mut had_hot) = (0usize, 0usize);
    for k in keys() {
        let before = plain.place(&k, R);
        let after = skipped.place(&k, R);
        if before.contains(&hot) {
            had_hot += 1;
            assert!(
                !after.contains(&hot),
                "the hot node was still placed for {k}"
            );
        } else if before != after {
            changed += 1;
        }
    }
    assert!(
        had_hot > 0,
        "the hot node held nothing, so nothing was shed"
    );
    assert_eq!(
        changed, 0,
        "{changed} placements that never touched the hot node were disturbed"
    );
}

#[test]
fn skipping_never_returns_fewer_than_r() {
    // `routing-and-placement.md` step 5: relax until R qualify, **never fail**. A short list
    // silently under-replicates, and nothing downstream would report it.
    let r = nodes(100);
    let all: Vec<String> = (0..100).map(|i| format!("n{i}")).collect();
    let p = Placement::new(&r).with_overloaded(all.iter().map(String::as_str));
    for k in keys().iter().take(1_000) {
        assert_eq!(p.place(k, R).len(), R, "key {k} placed on fewer than {R}");
    }
}

#[test]
fn a_small_fleet_places_on_everyone_it_has() {
    // Fewer nodes than R is legal — a fleet starting up, or one that lost most of itself —
    // and must return what exists rather than repeating a node to pad the list.
    let r = nodes(2);
    let p = Placement::new(&r);
    let got = p.place("idx1/s0", R);
    assert_eq!(got.len(), 2);
    assert_ne!(got[0], got[1], "a node was repeated to pad the list");
    assert!(Placement::new(&nodes(0)).place("k", R).is_empty());
}
