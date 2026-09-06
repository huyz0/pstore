//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The roster (D-4): the blob store as seed and partition-healing backstop.
//!
//! ⚠️ **A cache of gossip, not the source of truth.** A stale roster costs a slower join,
//! never a wrong membership — which is what lets it be refolded optimistically with no lease
//! and no leader.

use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_cluster::{Roster, RosterError};
use pstore_testkit::flaky::Flaky;
use pstore_types::TenantId;
use std::sync::Arc;

const CLUSTER: &str = "c1";

#[tokio::test]
async fn reading_the_roster_costs_one_get_and_no_list() {
    // ⚠️ Discovery must not scale with fleet size. A LIST is priced like a PUT, returns at
    // most 1000 keys, and would make joining cost more as the cluster grows — which is the
    // opposite of what a roster is for.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(1);
    let v = s.as_tenant(t);
    Roster::refold(
        &v,
        CLUSTER,
        &Roster::from_nodes(["a".into(), "b".into()]),
        None,
    )
    .await
    .unwrap();

    let before = (s.count(t, OpClass::Read), s.count(t, OpClass::List));
    let (r, tag) = Roster::read(&v, CLUSTER).await.unwrap();
    assert_eq!(
        s.count(t, OpClass::Read) - before.0,
        1,
        "join cost more than one GET"
    );
    assert_eq!(s.count(t, OpClass::List) - before.1, 0, "join listed");
    assert_eq!(r.nodes(), ["a", "b"]);
    assert!(tag.is_some());
}

#[tokio::test]
async fn a_cold_cluster_reads_an_empty_fleet_not_an_error() {
    // The first node to start must be able to join a cluster that does not exist yet.
    let s = MemoryStore::new();
    let (r, tag) = Roster::read(&s, "brand-new").await.unwrap();
    assert!(r.nodes().is_empty());
    assert!(
        tag.is_none(),
        "a missing roster reported a tag to condition on"
    );
}

#[tokio::test]
async fn a_lost_refold_rebases_rather_than_overwriting() {
    // ⚠️ The property that makes a leaderless refold safe. Two nodes refold from views taken
    // at the same moment; the loser must NOT retry its own bytes, because those bytes do not
    // contain whatever the winner just recorded. Rebase and merge, or lose members silently.
    //
    // Asserted against the in-process store, where `Lost` is distinguishable. MinIO ignores
    // the `If-None-Match: *` wildcard -- this project's own conformance suite measured it --
    // so a cold cluster's first roster write is exactly the race that primitive does not
    // survive there, and a green test on an emulator would mean nothing.
    let s = MemoryStore::new();
    let seed = Roster::from_nodes(["a".into()]);
    Roster::refold(&s, CLUSTER, &seed, None).await.unwrap();
    let (base, tag) = Roster::read(&s, CLUSTER).await.unwrap();

    // Node one wins, adding "b".
    let one = base.merged(&Roster::from_nodes(["b".into()]));
    Roster::refold(&s, CLUSTER, &one, tag.clone())
        .await
        .unwrap();

    // Node two, holding the same stale tag, adds "c" and must lose.
    let two = base.merged(&Roster::from_nodes(["c".into()]));
    assert!(
        matches!(
            Roster::refold(&s, CLUSTER, &two, tag).await,
            Err(RosterError::Lost)
        ),
        "a stale refold was accepted, which drops the winner's members"
    );

    // Rebasing keeps both.
    let (now, tag2) = Roster::read(&s, CLUSTER).await.unwrap();
    let merged = now.merged(&two);
    Roster::refold(&s, CLUSTER, &merged, tag2).await.unwrap();
    let (fin, _) = Roster::read(&s, CLUSTER).await.unwrap();
    assert_eq!(
        fin.nodes(),
        ["a", "b", "c"],
        "a member was lost to the race"
    );
}

#[tokio::test]
async fn a_hundred_concurrent_refolds_are_bounded() {
    // ⚠️ M4's first draft budgeted "1 CAS per cluster, by whichever node wins" and ignored
    // the 99 losers. With no leader permitted -- no lease is allowed for correctness -- every
    // node attempts, so the real cost is per node, and unbounded retry multiplies exactly the
    // contention it is meant to survive.
    let s = Accounted::new(MemoryStore::new());
    let t = TenantId(2);
    let v = Arc::new(s.as_tenant(t));
    Roster::refold(&*v, CLUSTER, &Roster::from_nodes(["seed".into()]), None)
        .await
        .unwrap();

    let before = s.count(t, OpClass::Read) + s.count(t, OpClass::Write);
    let mut tasks = Vec::new();
    for i in 0..100 {
        let v = Arc::clone(&v);
        tasks.push(tokio::spawn(async move {
            // One read, one attempt, one rebase-and-retry. Bounded on purpose.
            for _ in 0..2 {
                let (cur, tag) = Roster::read(&*v, CLUSTER).await.unwrap();
                let mine = cur.merged(&Roster::from_nodes([format!("n{i}")]));
                if Roster::refold(&*v, CLUSTER, &mine, tag).await.is_ok() {
                    return;
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let spent = s.count(t, OpClass::Read) + s.count(t, OpClass::Write) - before;
    assert!(
        spent <= 500,
        "100 nodes refolding spent {spent} blob requests; at 10,000 nodes that rate lands on \
         one key and exceeds what the CAS sensitivity sweep found survivable"
    );
    assert_eq!(s.count(t, OpClass::List), 0, "a refold listed");
}

#[tokio::test]
async fn a_corrupt_roster_is_refused_not_guessed() {
    // A roster that decodes to nonsense would place every key on nodes that do not exist,
    // and every query would fail somewhere else entirely.
    let s = MemoryStore::new();
    s.put(&Roster::key(CLUSTER), bytes::Bytes::from(vec![0xff, 0xfe]))
        .await
        .unwrap();
    assert!(matches!(
        Roster::read(&s, CLUSTER).await,
        Err(RosterError::Corrupt(_))
    ));
}

#[tokio::test]
async fn a_failed_read_is_an_error_not_an_empty_fleet() {
    // ⚠️ The most dangerous confusion available here. `NotFound` means "cold cluster";
    // anything else means "we do not know". Collapsing them makes a node on a struggling
    // backend join an empty fleet, place every key on itself, and report success.
    let s = Flaky::refusing_reads();
    assert!(matches!(
        Roster::read(&s, CLUSTER).await,
        Err(RosterError::Blob(_))
    ));
}

#[tokio::test]
async fn the_roster_key_is_derived_not_discovered() {
    // No lookup, no catalog, no DNS: the key falls out of the cluster name.
    assert_eq!(Roster::key("c1"), Roster::key("c1"));
    assert_ne!(Roster::key("c1"), Roster::key("c2"));
    assert!(Roster::key("c1").as_str().ends_with("/clu/ROSTER"));
}
