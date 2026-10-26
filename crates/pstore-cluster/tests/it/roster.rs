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
use pstore_cluster::{Cell, Roster, RosterError};
use pstore_testkit::broken::{Broken, Defect};
use pstore_testkit::flaky::Flaky;
use pstore_types::TenantId;
use std::sync::Arc;

/// The cell these tests operate on. ⚠️ A cell, not a cluster: D-79 gives each AZ its own ring
/// and `Roster` no longer has an address that omits one.
fn cell() -> Cell {
    Cell::new("c1", "az-a")
}

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
        &cell(),
        &Roster::from_nodes(["a".into(), "b".into()]),
        None,
    )
    .await
    .unwrap();

    let before = (s.count(t, OpClass::Read), s.count(t, OpClass::List));
    let (r, tag) = Roster::read(&v, &cell()).await.unwrap();
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
    let (r, tag) = Roster::read(&s, &Cell::new("brand-new", "az-a"))
        .await
        .unwrap();
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
    Roster::refold(&s, &cell(), &seed, None).await.unwrap();
    let (base, tag) = Roster::read(&s, &cell()).await.unwrap();

    // Node one wins, adding "b".
    let one = base.merged(&Roster::from_nodes(["b".into()]));
    Roster::refold(&s, &cell(), &one, tag.clone())
        .await
        .unwrap();

    // Node two, holding the same stale tag, adds "c" and must lose.
    let two = base.merged(&Roster::from_nodes(["c".into()]));
    assert!(
        matches!(
            Roster::refold(&s, &cell(), &two, tag).await,
            Err(RosterError::Lost)
        ),
        "a stale refold was accepted, which drops the winner's members"
    );

    // Rebasing keeps both.
    let (now, tag2) = Roster::read(&s, &cell()).await.unwrap();
    let merged = now.merged(&two);
    Roster::refold(&s, &cell(), &merged, tag2).await.unwrap();
    let (fin, _) = Roster::read(&s, &cell()).await.unwrap();
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
    Roster::refold(&*v, &cell(), &Roster::from_nodes(["seed".into()]), None)
        .await
        .unwrap();

    let before = s.count(t, OpClass::Read) + s.count(t, OpClass::Write);
    let mut tasks = Vec::new();
    for i in 0..100 {
        let v = Arc::clone(&v);
        tasks.push(tokio::spawn(async move {
            // One read, one attempt, one rebase-and-retry. Bounded on purpose.
            for _ in 0..2 {
                let (cur, tag) = Roster::read(&*v, &cell()).await.unwrap();
                let mine = cur.merged(&Roster::from_nodes([format!("n{i}")]));
                if Roster::refold(&*v, &cell(), &mine, tag).await.is_ok() {
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
    s.put(&Roster::key(&cell()), bytes::Bytes::from(vec![0xff, 0xfe]))
        .await
        .unwrap();
    assert!(matches!(
        Roster::read(&s, &cell()).await,
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
        Roster::read(&s, &cell()).await,
        Err(RosterError::Blob(_))
    ));
}

#[tokio::test]
async fn the_roster_key_is_derived_not_discovered() {
    // No lookup, no catalog, no DNS: the key falls out of the cell.
    // ⚠️ Injectivity across a generated set lives in `tests/cell.rs`; two hand-picked
    // inequalities cannot see a collision, which is how the old 16-bit key survived M4a.
    let a = Cell::new("c1", "az-a");
    assert_eq!(Roster::key(&a), Roster::key(&Cell::new("c1", "az-a")));
    assert_ne!(Roster::key(&a), Roster::key(&Cell::new("c2", "az-a")));
    assert_ne!(Roster::key(&a), Roster::key(&Cell::new("c1", "az-b")));
    assert!(Roster::key(&a).as_str().ends_with("/ROSTER"));
}

#[tokio::test]
async fn a_broken_cas_delays_convergence_it_does_not_lose_a_member() {
    // ⚠️ **The test the roster's exemption from M7a's fencing guard rests on.** Every
    // conditional write in `pstore-engine` and `pstore-catalog` now refuses on a backend that
    // cannot fence; this one does not, and the reason is that losing the race here is
    // recoverable and losing it there is not.
    //
    // `IgnoresCreateIfAbsent` is MinIO's documented defect: the second create succeeds and
    // overwrites. So node A's members really are gone from the object -- and the claim is
    // only that the *next* refold from a merged view brings them back, because the roster is
    // a union and never a replacement.
    let s = Broken::new(Defect::IgnoresCreateIfAbsent);
    let a = Roster::from_nodes(["a".into()]);
    let b = Roster::from_nodes(["b".into()]);

    Roster::refold(&s, &cell(), &a, None).await.unwrap();
    // No fencing: this lands even though the object already exists.
    Roster::refold(&s, &cell(), &b, None).await.unwrap();
    let (stored, tag) = Roster::read(&s, &cell()).await.unwrap();
    assert_eq!(
        stored.nodes().len(),
        1,
        "the fixture must actually have lost a member, or it proves nothing"
    );

    // One round of gossip later, A refolds from what it knows merged with what it reads.
    Roster::refold(&s, &cell(), &stored.merged(&a), tag)
        .await
        .unwrap();
    let (converged, _) = Roster::read(&s, &cell()).await.unwrap();
    let mut ids: Vec<_> = converged.nodes().iter().map(ToString::to_string).collect();
    ids.sort();
    assert_eq!(ids, vec!["a".to_owned(), "b".to_owned()]);
}
