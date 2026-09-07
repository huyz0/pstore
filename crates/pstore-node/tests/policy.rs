//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The decisions a node makes, tested without a cluster.
//!
//! ⚠️ Every case here is one a 100-node Docker fleet found first, at a cost of minutes per
//! run and a diagnosis that looked like a gossip bug each time. That is the argument for the
//! library: these are branches, and branches belong in tests.

use pstore_blob::MemoryStore;
use pstore_cluster::{Cell, Roster};
use pstore_node::policy::{
    fresh_node_id, jitter, owned_shards, read_roster_patiently, to_dial, union_to_publish,
};
use pstore_testkit::flaky::Flaky;

fn nodes(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("10.0.0.{i}:7946")).collect()
}

#[test]
fn a_node_dials_exactly_what_it_cannot_see() {
    let known = Roster::from_nodes(nodes(5));
    let view = vec!["10.0.0.1:7946".to_owned(), "10.0.0.3:7946".to_owned()];
    let mut got = to_dial(&known, &view);
    got.sort_unstable();
    assert_eq!(got, ["10.0.0.0:7946", "10.0.0.2:7946", "10.0.0.4:7946"]);
}

#[test]
fn a_node_that_sees_everyone_dials_nobody() {
    // The steady state, and the one that must cost nothing: a converged fleet sending a
    // datagram per peer per heal period would scale traffic with nodes for no information.
    let known = Roster::from_nodes(nodes(50));
    assert!(to_dial(&known, &nodes(50)).is_empty());
}

#[test]
fn an_isolated_node_dials_the_whole_roster() {
    // ⚠️ The case the backstop exists for. Measured before it did: a node whose seeds were
    // all unreachable at startup held a one-member view for 493 ticks while 96 peers
    // converged without it, and nothing in the protocol could recover it.
    let known = Roster::from_nodes(nodes(100));
    let alone = vec!["10.0.0.7:7946".to_owned()];
    assert_eq!(to_dial(&known, &alone).len(), 99);
}

#[test]
fn publishing_adds_what_the_roster_is_missing() {
    let known = Roster::from_nodes(nodes(3));
    let view = vec!["10.0.0.9:7946".to_owned(), "10.0.0.1:7946".to_owned()];
    let out = union_to_publish(&known, &view).expect("a new member must be published");
    assert_eq!(out.nodes().len(), 4, "the union must keep both sides");
    assert!(out.nodes().iter().any(|n| n == "10.0.0.9:7946"));
    assert!(
        out.nodes().iter().any(|n| n == "10.0.0.2:7946"),
        "publishing the view alone would drop a member the roster knew"
    );
}

#[test]
fn a_minority_view_still_cannot_shrink_the_roster() {
    // ⚠️ The failure that stalled a 100-node fleet at 45 members. When refold published the
    // node's own view, the roster could only ever be as large as ONE node's view — and a
    // node missing from it was dialled by nobody, because nobody held its address. The union
    // is what makes the roster a directory rather than a snapshot of whoever wrote last.
    let known = Roster::from_nodes(nodes(100));
    let isolated = vec!["10.0.0.7:7946".to_owned()];
    assert!(
        union_to_publish(&known, &isolated).is_none(),
        "a one-member view must add nothing, and must certainly not replace 100 members"
    );
}

#[test]
fn a_converged_fleet_writes_nothing() {
    // Returning `None` is what keeps the steady-state PUT rate at zero. A node that
    // republished an identical roster every period would put one write per node per period
    // on a single key — a cost that scales with nodes and carries no information.
    let known = Roster::from_nodes(nodes(20));
    assert!(union_to_publish(&known, &nodes(20)).is_none());
}

#[test]
fn a_node_owns_a_fair_share_and_finds_itself() {
    // ⚠️ Measured on a live fleet: every node reported owning 0 of 1000 shards while holding
    // a 44-member view. Placement was correct; the node was comparing its CONFIGURED name
    // against a view that holds the address gossip resolved and advertises.
    let roster = Roster::from_nodes(nodes(10));
    let mine = owned_shards(&roster, "10.0.0.3:7946", 1000, 3);
    let fair = 1000 * 3 / 10;
    assert!(
        (fair / 2..fair * 2).contains(&mine),
        "a node in a 10-node fleet holds {mine} of 3000 replicas, against a fair share of {fair}"
    );
    assert_eq!(
        owned_shards(&roster, "10.9.9.9:7946", 1000, 3),
        0,
        "a node not in the roster owns nothing, and must not silently match"
    );
}

#[test]
fn jitter_spreads_the_fleet_across_the_period() {
    // An unjittered period puts every node on one key at the same instant. With 100 ids the
    // slots must actually be spread, not merely computed.
    // ⚠️ The defining property first: a slot is a position INSIDE the period. Without this
    // the test asserted only spread and two edge cases, and mutation testing showed `%` could
    // become `/` or `+` — both returning values far outside the period, both leaving a node
    // scheduling its roster write at an offset that does not exist — with everything green.
    for period in [1_u64, 2, 10, 60, 3600] {
        for i in 0..200 {
            let slot = jitter(&format!("node-{i}"), period);
            assert!(
                slot < period,
                "jitter gave slot {slot} for a period of {period}: a node cannot act at an \
                 offset the period does not contain"
            );
        }
    }

    let slots: std::collections::HashSet<u64> =
        (0..100).map(|i| jitter(&format!("node-{i}"), 10)).collect();
    assert!(
        slots.len() >= 8,
        "100 nodes landed in only {} of 10 slots",
        slots.len()
    );
    assert_eq!(
        jitter("node-7", 10),
        jitter("node-7", 10),
        "not reproducible"
    );
    assert_eq!(jitter("x", 1), 0, "a one-slot period is the only slot");
    assert_eq!(jitter("x", 0), 0, "a zero period must not divide by zero");
}

#[test]
fn an_identity_is_fresh_on_every_start() {
    // ⚠️ A restarted node must look COLD: it owns nothing and holds no cache. An identity
    // derived from IP or hostname would tell the fleet otherwise.
    let a = fresh_node_id();
    let b = fresh_node_id();
    assert_ne!(a, b, "two starts produced the same identity");
    assert_eq!(a.len(), 32);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
}

#[tokio::test]
async fn a_join_reads_the_roster_once_and_tolerates_an_empty_one() {
    let store = MemoryStore::new();
    let (roster, _) = read_roster_patiently(&store, &Cell::new("c1", "az-a"), "n1")
        .await
        .unwrap();
    assert!(
        roster.nodes().is_empty(),
        "a cold cluster's roster is empty, not an error: the first node has to be able to join"
    );
}

#[tokio::test]
async fn a_join_returns_the_members_already_present() {
    let store = MemoryStore::new();
    let seeded = Roster::from_nodes(nodes(4));
    Roster::refold(&store, &Cell::new("c1", "az-a"), &seeded, None)
        .await
        .unwrap();

    let (roster, tag) = read_roster_patiently(&store, &Cell::new("c1", "az-a"), "n9")
        .await
        .unwrap();
    assert_eq!(roster.nodes().len(), 4);
    assert!(tag.is_some(), "an existing roster must carry a CAS tag");
}

#[tokio::test(start_paused = true)]
async fn a_join_gives_up_rather_than_retrying_a_dead_store_forever() {
    // ⚠️ A node that cannot reach the roster has not learned the fleet is empty — it has
    // learned nothing — so it retries. But the budget has to END, or a misconfigured fleet
    // reports nothing at all rather than an error. `Flaky::refusing_reads` is the store that
    // is never coming back.
    let err = read_roster_patiently(&Flaky::refusing_reads(), &Cell::new("c1", "az-a"), "n1")
        .await
        .unwrap_err();
    assert!(
        err.contains("attempts"),
        "a store that always refuses must produce a give-up error, got: {err}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_retry_schedule_actually_backs_off() {
    // ⚠️ Mutation testing found this hole: `<<` could become `>>`, and `base + jitter` could
    // become `base - jitter` or `base * jitter`, with every test still green. The schedule
    // is the ONLY thing standing between a cold fleet and the herd that took it down —
    // measured, 56 of 100 nodes exhausted their retries in 58s and exited — so a test that
    // checks only "it gives up eventually" is checking the wrong half.
    //
    // On a paused clock the virtual elapsed time IS the schedule, exactly and for free.
    let start = tokio::time::Instant::now();
    let _ =
        read_roster_patiently(&Flaky::refusing_reads(), &Cell::new("c1", "az-a"), "node-a").await;
    let elapsed = start.elapsed();

    // Twelve attempts: 250ms doubling to a 8s ceiling, plus a per-node jitter of up to the
    // same again, plus twelve 5s timeouts. A schedule that shrank instead of growing, or
    // that subtracted its jitter, lands far below this floor.
    assert!(
        elapsed >= std::time::Duration::from_secs(60),
        "the whole retry budget took {elapsed:?}: that is not a backoff, and a fleet of them          is the herd the backoff exists to break up"
    );
    assert!(
        elapsed <= std::time::Duration::from_secs(300),
        "the retry budget took {elapsed:?}: a joining node that waits this long is one the          operator has already declared dead"
    );

    // And the jitter must actually differ between nodes, or the fleet retries in lockstep
    // and the backoff spreads nothing.
    let other = tokio::time::Instant::now();
    let _ =
        read_roster_patiently(&Flaky::refusing_reads(), &Cell::new("c1", "az-a"), "node-b").await;
    assert_ne!(
        other.elapsed(),
        elapsed,
        "two different node ids drew the same schedule, so the jitter is not reaching it"
    );
}

#[tokio::test(start_paused = true)]
async fn a_join_survives_a_store_that_is_briefly_unreachable() {
    // The other half, and the one that matters more: a cold fleet IS a thundering herd —
    // measured, 56 of 100 nodes exhausted their client's retries in 58s and exited, leaving
    // the fleet stable at 44. Transient refusal must not cost a member.
    let store = Flaky::refusing_reads_at(&[0, 1, 2]);
    let (roster, _) = read_roster_patiently(&store, &Cell::new("c1", "az-a"), "n1")
        .await
        .expect("three refusals is well inside the budget");
    assert!(roster.nodes().is_empty());
    assert!(
        store.failures() >= 3,
        "only {} refusals were injected",
        store.failures()
    );
}

#[test]
fn a_node_without_a_zone_refuses_to_start() {
    // ⚠️ Required, with no default. D-79 gives each AZ its own placement ring, so a node with
    // no zone joins the wrong cell — and that is silent: placement still answers, queries
    // still return, and every cross-AZ byte is billed at $0.02/GB round trip. Nothing
    // observable goes wrong until the invoice.
    //
    // ⚠️ Tested against the pure decision rather than the environment, because mutating the
    // process env is `unsafe` in this edition and `unsafe_code = "forbid"` is a workspace
    // non-negotiable. A guard that can only be exercised by setting a variable is a guard
    // that cannot be tested at all.
    let err = pstore_node::policy::zone(None).unwrap_err();
    assert!(
        err.contains("PSTORE_AZ"),
        "the error must name the variable an operator has to set, got: {err}"
    );
    assert!(
        pstore_node::policy::zone(Some("   ")).is_err(),
        "a blank zone was accepted, which is a cell nobody is in"
    );
    assert!(
        pstore_node::policy::zone(Some("")).is_err(),
        "an empty zone was accepted"
    );
    assert_eq!(pstore_node::policy::zone(Some("az-b")).unwrap(), "az-b");
}
