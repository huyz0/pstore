//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Sweeping an orphan run — M6e.
//!
//! ⚠️ **The failure this must not cause is worse than the one it fixes.** An orphan is
//! garbage; a run a fold is *about to* commit is a bucket's worth of tenants. They sit in the
//! same prefix, and a sweeper that cannot tell them apart deletes the second — after which the
//! winning CAS publishes a head naming an object that no longer exists, and
//! `a_pointer_to_a_run_that_is_gone_is_an_error` turns every enumeration of that bucket into
//! `MissingRun`.
//!
//! The epoch in the key settles it with no clock: a run in flight was written against the head
//! its writer read, so its epoch is strictly **greater** than the head's.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_catalog::{
    Appender, CatalogError, TenantRecord, Width, enumerate, fold, head_key, read_head, reap,
    run_key, sweep,
};
use pstore_types::{Epoch, TenantId};
use std::sync::Arc;

fn one() -> Width {
    Width::new(1).expect("width 1")
}

fn rec(t: u128, e: u64) -> TenantRecord {
    TenantRecord::live(TenantId(t), Epoch(e), &["idx".to_owned()])
}

/// Appends one record and folds, `n` times.
async fn folds<S: BlobStore>(store: &Arc<S>, n: u64) -> Vec<Key> {
    let mut runs = Vec::new();
    for i in 0..n {
        let a = Appender::new(Arc::clone(store), one());
        a.record(&rec(u128::from(i), i)).await.unwrap();
        fold(store.as_ref(), 0).await.unwrap();
        let (head, _) = read_head(store.as_ref(), 0).await.unwrap();
        runs.push(head.run(0).expect("a run was folded"));
    }
    runs
}

/// An object at a run key nothing names — what a fold that lost its head CAS leaves behind.
async fn orphan<S: BlobStore>(store: &S, epoch: u64, digest: u64) -> Key {
    let k = run_key(0, Epoch(epoch), digest);
    store
        .put(&k, bytes::Bytes::from_static(b"orphan"))
        .await
        .unwrap();
    k
}

#[tokio::test]
async fn an_orphan_run_is_swept_and_the_live_one_is_not() {
    let store = Arc::new(MemoryStore::new());
    let runs = folds(&store, 3).await;
    // The head is at epoch 3; an object at epoch 2 that no head and no graveyard names.
    let dead = orphan(store.as_ref(), 2, 0xdead_beef_dead_beef).await;
    reap(store.as_ref(), 0, 0).await.unwrap();

    let n = sweep(store.as_ref(), 0).await.unwrap();
    assert_eq!(n, 1, "expected the one orphan to be swept");
    assert!(store.get(&dead).await.is_err(), "the orphan survived");
    assert!(
        store.get(&runs[2]).await.is_ok(),
        "the LIVE run was swept -- this bucket's tenants are gone"
    );
    assert_eq!(
        enumerate(store.as_ref(), one())
            .await
            .unwrap()
            .records
            .len(),
        3
    );
}

#[tokio::test]
async fn a_run_in_flight_survives_the_sweep() {
    // ⚠️ The milestone. A fold that has written its run and not yet committed looks exactly
    // like an orphan: an object at a run key nothing names. The difference is the EPOCH — it
    // is one past the head, because the writer read that head — and comparing on `<=` instead
    // of `<` deletes the object a winning CAS is about to publish.
    let store = Arc::new(MemoryStore::new());
    let runs = folds(&store, 2).await;
    let (head, _) = read_head(store.as_ref(), 0).await.unwrap();
    let in_flight = orphan(store.as_ref(), head.run_epoch.0 + 1, 0x1234).await;
    // And the loser of a same-epoch race, which is equal to the head's epoch, not less.
    let same_epoch = orphan(store.as_ref(), head.run_epoch.0, 0x5678).await;

    let n = sweep(store.as_ref(), 0).await.unwrap();
    assert_eq!(n, 0, "the sweep reaped {n} object(s) it should not have");
    assert!(
        store.get(&in_flight).await.is_ok(),
        "a run a fold is about to commit was deleted"
    );
    assert!(
        store.get(&same_epoch).await.is_ok(),
        "a same-epoch CAS loser was deleted in the same sweep that could not know it had lost"
    );
    assert!(store.get(&runs[1]).await.is_ok());
}

#[tokio::test]
async fn the_graveyards_runs_survive_the_sweep() {
    // ⚠️ M6d keeps the newest `retention` superseded runs so a reader mid-enumeration does not
    // lose the one it is on. Those are unnamed by the head and below its epoch — which is the
    // sweeper's definition of garbage — so a sweeper that does not consult the graveyard reaps
    // exactly the runs the retention window promised to keep.
    let store = Arc::new(MemoryStore::new());
    let runs = folds(&store, 3).await;
    let (head, _) = read_head(store.as_ref(), 0).await.unwrap();
    assert_eq!(
        head.graveyard.len(),
        2,
        "the fixture has no graveyard to protect"
    );

    assert_eq!(sweep(store.as_ref(), 0).await.unwrap(), 0);
    for r in &runs {
        assert!(
            store.get(r).await.is_ok(),
            "a run inside the retention window was swept"
        );
    }
}

#[tokio::test]
async fn an_unrecognised_object_under_the_prefix_survives() {
    // ⚠️ The default is "keep what I cannot identify". The opposite -- "delete what I do not
    // recognise" -- deletes the bucket's own pointer, and it is one edit away. Asserted with
    // the head AND a foreign object, because special-casing `HEAD` by name passes the first
    // half while still deleting everything a later milestone puts beside it.
    let store = Arc::new(MemoryStore::new());
    folds(&store, 3).await;
    let foreign = Key::new(format!("{:04x}/cat/b/notes.txt", 0));
    store
        .put(&foreign, bytes::Bytes::from_static(b"hello"))
        .await
        .unwrap();
    let before = store.get(&head_key(0)).await.unwrap();

    sweep(store.as_ref(), 0).await.unwrap();
    assert!(
        store.get(&foreign).await.is_ok(),
        "an object the sweeper could not parse was deleted"
    );
    assert_eq!(
        store.get(&head_key(0)).await.unwrap(),
        before,
        "the bucket's pointer was deleted or rewritten"
    );
}

#[tokio::test]
async fn a_second_sweep_finds_nothing_and_changes_nothing() {
    let store = Arc::new(MemoryStore::new());
    folds(&store, 3).await;
    orphan(store.as_ref(), 1, 0xaaaa).await;
    reap(store.as_ref(), 0, 0).await.unwrap();
    let before = store.get(&head_key(0)).await.unwrap();

    assert_eq!(sweep(store.as_ref(), 0).await.unwrap(), 1);
    assert_eq!(sweep(store.as_ref(), 0).await.unwrap(), 0);
    assert_eq!(
        store.get(&head_key(0)).await.unwrap(),
        before,
        "the sweeper wrote the head -- it records nothing and must contend for nothing"
    );
}

#[tokio::test]
async fn a_sweep_is_one_list_and_never_more() {
    let t = TenantId(0);
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let view = Arc::new(acct.as_tenant(t));
    folds(&view, 3).await;
    orphan(view.as_ref(), 1, 0xbbbb).await;
    reap(view.as_ref(), 0, 0).await.unwrap();
    let before = acct.count(t, OpClass::List);

    sweep(view.as_ref(), 0).await.unwrap();
    assert_eq!(
        acct.count(t, OpClass::List) - before,
        1,
        "a sweep costs one LIST per bucket -- more than that scales with something else"
    );
    // ⚠️ And nothing else in the catalog lists at all.
    enumerate(view.as_ref(), one()).await.unwrap();
    fold(view.as_ref(), 0).await.unwrap();
    assert_eq!(
        acct.count(t, OpClass::List) - before,
        1,
        "enumerate or fold listed"
    );
}

#[tokio::test]
async fn a_bucket_with_no_head_sweeps_nothing() {
    // ⚠️ An absent pointer reads as `Epoch::ZERO`, and nothing is strictly below zero. A first
    // fold that lost its create-if-absent leaves an orphan this cannot collect until some fold
    // succeeds -- safe, and stated rather than discovered.
    let store = Arc::new(MemoryStore::new());
    let stray = orphan(store.as_ref(), 1, 0xcccc).await;
    assert_eq!(sweep(store.as_ref(), 0).await.unwrap(), 0);
    assert!(store.get(&stray).await.is_ok());
}

#[tokio::test]
async fn a_sweep_on_a_divergent_backend_is_refused() {
    let acc = pstore_blob::Accounted::new(pstore_testkit::claims::Claims::divergent_cas(
        "wildcard ignored",
    ));
    let bill = TenantId(0);
    let store = Arc::new(acc.as_tenant(bill));
    let err = sweep(store.as_ref(), 0)
        .await
        .expect_err("a sweep ran against a backend that cannot fence");
    assert!(
        matches!(err, CatalogError::BackendCannotFence { .. }),
        "{err}"
    );
    assert_eq!(
        acc.count(bill, OpClass::Delete),
        0,
        "a refused sweep deleted anyway"
    );
}
