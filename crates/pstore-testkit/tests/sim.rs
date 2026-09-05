//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism, so the
//! workspace-wide bans on unwrap/expect/panic are lifted here and only here.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The simulator and the auditor have to be shown to work before anything is concluded
//! from them: a scheduler that ignores its seed, or an auditor that never fails, would let
//! every later result look like a proof.
use bytes::Bytes;
use pstore_blob::{BlobStore, Key, MemoryStore, Precondition};
use pstore_testkit::audit::Auditing;
use pstore_testkit::sim::Sim;

#[test]
fn a_seed_reproduces_the_same_schedule() {
    // A concurrency bug found once and never again is forgotten, not fixed.
    let trace = |seed| {
        let mut s = Sim::new(seed);
        let mut out = Vec::new();
        for _ in 0..200 {
            out.push((s.choose(7), s.chance(0.3)));
        }
        (out, s.steps())
    };
    assert_eq!(trace(42), trace(42), "the same seed must replay exactly");
}

#[test]
fn different_seeds_diverge() {
    // A scheduler that ignores its seed would make every run identical and every fault
    // injection a no-op.
    let trace = |seed| {
        let mut s = Sim::new(seed);
        (0..200).map(|_| s.choose(7)).collect::<Vec<_>>()
    };
    assert_ne!(trace(1), trace(2));
    assert_ne!(trace(0), trace(u64::MAX));
}

#[test]
fn choose_stays_in_range_and_shuffle_is_a_permutation() {
    let mut s = Sim::new(9);
    for n in [1usize, 2, 7, 100] {
        for _ in 0..200 {
            assert!(s.choose(n) < n);
        }
    }
    assert_eq!(s.choose(0), 0, "an empty choice must not divide by zero");

    let mut items: Vec<u32> = (0..50).collect();
    s.shuffle(&mut items);
    assert_ne!(
        items,
        (0..50).collect::<Vec<_>>(),
        "a shuffle that never moves is not one"
    );
    items.sort_unstable();
    assert_eq!(
        items,
        (0..50).collect::<Vec<_>>(),
        "shuffle must not lose or duplicate"
    );
}

#[test]
fn chance_respects_its_probability() {
    let mut s = Sim::new(3);
    let n = 10_000;
    let hits = (0..n).filter(|_| s.chance(0.25)).count();
    assert!((2000..3000).contains(&hits), "0.25 of {n} produced {hits}");
    let mut s = Sim::new(3);
    assert_eq!((0..100).filter(|_| s.chance(0.0)).count(), 0);
    let mut s = Sim::new(3);
    assert_eq!((0..100).filter(|_| s.chance(1.0)).count(), 100);
}

#[tokio::test]
async fn the_auditing_store_rejects_an_unconditional_overwrite() {
    // An auditor that never fails would make every I1 assertion vacuous.
    let a = Auditing::new(MemoryStore::new());
    let k = Key::new("x");
    a.put(&k, Bytes::from_static(b"1")).await.unwrap();
    assert!(
        a.violations().is_empty(),
        "the first write is a create, not a mutation"
    );
    a.put(&k, Bytes::from_static(b"2")).await.unwrap();
    assert_eq!(
        a.violations().len(),
        1,
        "the second is a mutation and must be caught"
    );
    assert_eq!(a.violations()[0].key, "x");
}

#[tokio::test]
async fn a_compare_and_swap_is_not_a_violation() {
    // The false-refusal path: CAS names the version it replaces, so it is exactly the
    // thing I1 permits. An auditor that flagged it would be switched off.
    let a = Auditing::new(MemoryStore::new());
    let k = Key::new("head");
    let first = a
        .put_conditional(&k, Bytes::from_static(b"1"), Precondition::NotExists)
        .await
        .unwrap();
    a.put_conditional(&k, Bytes::from_static(b"2"), Precondition::Match(first.tag))
        .await
        .unwrap();
    assert!(a.violations().is_empty(), "{:?}", a.violations());
    assert_eq!(a.keys_written(), 1);
}

#[tokio::test]
async fn a_reaped_key_may_be_written_again() {
    // GC is what makes a key free. Treating the next write as a mutation would make the
    // auditor and the collector permanently incompatible.
    let a = Auditing::new(MemoryStore::new());
    let k = Key::new("seg");
    a.put(&k, Bytes::from_static(b"1")).await.unwrap();
    a.delete_batch(std::slice::from_ref(&k)).await.unwrap();
    a.put(&k, Bytes::from_static(b"2")).await.unwrap();
    assert!(a.violations().is_empty(), "{:?}", a.violations());
}

#[tokio::test]
async fn the_auditor_forwards_the_whole_contract() {
    let a = Auditing::new(MemoryStore::new());
    let r = pstore_testkit::conformance::run(&a, 700).await;
    assert!(r.conforms(), "divergences: {:?}", r.divergences());
}
