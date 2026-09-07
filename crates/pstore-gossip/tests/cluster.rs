//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The member set, and the checksum that lets two nodes agree in eight bytes.
//!
//! ⚠️ The whole milestone rests on one property: **a converged cluster's checksum stops
//! changing.** `chitchat` cannot have that, because every node bumps a gossiped heartbeat
//! every round. Every test here is ultimately about protecting it.

use pstore_gossip::{Cluster, State};

fn id(n: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0] = n;
    b
}

fn addr(n: u8) -> String {
    format!("127.0.0.1:{}", 7000 + u16::from(n))
}

fn cluster_of(n: u8) -> Cluster {
    let mut c = Cluster::new(id(0), addr(0));
    for i in 1..n {
        c.join(id(i), addr(i));
    }
    c
}

#[test]
fn a_new_cluster_contains_only_itself() {
    let c = Cluster::new(id(0), addr(0));
    assert_eq!(c.len(), 1);
    assert_eq!(c.alive().len(), 1, "a node must consider itself alive");
}

#[test]
fn the_checksum_of_a_stable_cluster_does_not_change() {
    // ⚠️ THE property. If this can drift while nothing happens, checksum-first saves nothing
    // and this crate has no reason to exist — which is exactly the state `chitchat` is in,
    // because its heartbeat is part of the state a digest covers.
    let c = cluster_of(50);
    let first = c.checksum();
    for _ in 0..1_000 {
        assert_eq!(
            c.checksum(),
            first,
            "the checksum moved while the cluster did nothing"
        );
    }
}

#[test]
fn two_clusters_that_agree_have_the_same_checksum() {
    // Order of joins must not matter: two nodes learn of members in different orders and
    // still have to recognise agreement, or they reconcile forever.
    let mut a = Cluster::new(id(0), addr(0));
    let mut b = Cluster::new(id(0), addr(0));
    for i in 1..30 {
        a.join(id(i), addr(i));
    }
    for i in (1..30).rev() {
        b.join(id(i), addr(i));
    }
    assert_eq!(
        a.checksum(),
        b.checksum(),
        "join order changed the checksum"
    );
}

#[test]
fn any_difference_changes_the_checksum() {
    // A checksum that misses a difference is worse than none: the two nodes agree to stay
    // divergent, permanently and silently.
    let base = cluster_of(20);

    let mut extra = cluster_of(20);
    extra.join(id(99), addr(99));
    assert_ne!(
        base.checksum(),
        extra.checksum(),
        "an added member was invisible"
    );

    let mut suspected = cluster_of(20);
    suspected.suspect(&id(5));
    assert_ne!(
        base.checksum(),
        suspected.checksum(),
        "a suspicion was invisible"
    );

    let mut dead = cluster_of(20);
    dead.declare_dead(&id(5));
    assert_ne!(base.checksum(), dead.checksum(), "a death was invisible");

    let mut refuted = cluster_of(20);
    refuted.suspect(&id(5));
    refuted.refute(&id(5), 1);
    assert_ne!(
        suspected.checksum(),
        refuted.checksum(),
        "a refutation was invisible"
    );
}

#[test]
fn the_checksum_is_maintained_incrementally() {
    // ⚠️ Reading the checksum must be O(1). Re-hashing every member each round moves the cost
    // rather than removing it — and at 1,000 nodes that is the cost this crate exists to
    // remove. Compared against a from-scratch computation so the incremental path cannot
    // drift from the definition.
    let mut c = Cluster::new(id(0), addr(0));
    for i in 1..40 {
        c.join(id(i), addr(i));
        assert_eq!(
            c.checksum(),
            c.checksum_from_scratch(),
            "incremental checksum diverged after joining {i}"
        );
    }
    for i in (1..20).step_by(3) {
        c.suspect(&id(i));
        assert_eq!(
            c.checksum(),
            c.checksum_from_scratch(),
            "diverged after suspect"
        );
        c.declare_dead(&id(i));
        assert_eq!(
            c.checksum(),
            c.checksum_from_scratch(),
            "diverged after death"
        );
    }
}

#[test]
fn a_wrongly_suspected_node_refutes_and_returns() {
    // ⚠️ Without incarnation a false suspicion is PERMANENT: the suspected node can never
    // say otherwise, and one slow probe removes it from the fleet forever.
    let mut c = cluster_of(10);
    c.suspect(&id(3));
    assert_eq!(c.state(&id(3)), Some(State::Suspect));

    // A refutation at the same incarnation is not evidence — it is what a replayed old
    // message looks like.
    c.refute(&id(3), 0);
    assert_eq!(
        c.state(&id(3)),
        Some(State::Suspect),
        "a refutation at the same incarnation was accepted, so a replay can resurrect a node"
    );

    c.refute(&id(3), 1);
    assert_eq!(
        c.state(&id(3)),
        Some(State::Alive),
        "a valid refutation was ignored"
    );
}

#[test]
fn a_dead_node_stays_dead_against_a_stale_message() {
    // Death is not refutable at a lower incarnation, or a delayed packet resurrects a node
    // the fleet already routed away from.
    let mut c = cluster_of(10);
    c.suspect(&id(4));
    c.declare_dead(&id(4));
    c.refute(&id(4), 0);
    assert_eq!(c.state(&id(4)), Some(State::Dead));
}

#[test]
fn a_node_never_suspects_itself() {
    // ⚠️ A node that accepts a suspicion of itself removes itself from the fleet on the word
    // of a peer whose own probe loop was slow -- OQ-12's failure, arrived at from the other
    // side.
    let mut c = cluster_of(10);
    c.suspect(&id(0));
    assert_eq!(
        c.state(&id(0)),
        Some(State::Alive),
        "a node accepted a suspicion of itself"
    );
}

#[test]
fn joining_a_member_twice_does_not_double_count_it() {
    let mut c = cluster_of(10);
    let before = (c.len(), c.checksum());
    c.join(id(5), addr(5));
    assert_eq!((c.len(), c.checksum()), before, "a rejoin changed the set");
}

#[test]
fn alive_excludes_the_dead_and_the_suspected_do_not_vanish() {
    let mut c = cluster_of(10);
    c.suspect(&id(2));
    c.declare_dead(&id(3));
    let alive = c.alive();
    assert!(
        alive.iter().any(|m| m.id == id(2)),
        "a suspect is not yet dead"
    );
    assert!(
        !alive.iter().any(|m| m.id == id(3)),
        "a dead node was reported alive"
    );
    assert_eq!(c.len(), 10, "a dead node is remembered, not forgotten");
}
