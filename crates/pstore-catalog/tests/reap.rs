//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Reaping a superseded run — M6d.
//!
//! ⚠️ **A run's key is derived, not remembered.** `run_key(bucket, run_epoch, digest)` needs a
//! content digest, and the head carries only the digest of the run it names — so an old run's
//! key cannot be computed from the current head, and garbage nobody can name is garbage
//! forever. That is why there is a graveyard at all.
//!
//! ⚠️ Reaping too early is **loud**: `a_pointer_to_a_run_that_is_gone_is_an_error` already
//! pins that a missing run raises `MissingRun` rather than reading as an empty bucket. That is
//! what makes a retention window a safe knob and not a silent correctness risk.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_catalog::{
    Appender, BucketHead, CatalogError, MAX_GRAVEYARD, TenantRecord, Width, enumerate, fold,
    read_head, reap,
};
use pstore_testkit::flaky::Flaky;
use pstore_types::{Epoch, TenantId};
use std::sync::Arc;

fn one() -> Width {
    Width::new(1).expect("width 1")
}

fn rec(t: u128, e: u64) -> TenantRecord {
    TenantRecord::live(TenantId(t), Epoch(e), &["idx".to_owned()])
}

/// Appends `n` records and folds, `n` times — so the bucket has `n` runs in its history.
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

#[tokio::test]
async fn a_superseded_run_is_reaped_and_the_live_one_is_not() {
    let store = Arc::new(MemoryStore::new());
    let runs = folds(&store, 2).await;
    assert_ne!(
        runs[0], runs[1],
        "the second fold reused the first run's key"
    );

    let n = reap(store.as_ref(), 0, 0).await.unwrap();
    assert_eq!(n, 1, "expected the one superseded run to be reaped");
    assert!(
        store.get(&runs[0]).await.is_err(),
        "the superseded run survived"
    );
    // ⚠️ The live run is never a graveyard entry, so keeping none of the dead ones cannot
    // touch it -- and if it could, this is where the catalog loses a bucket of tenants.
    assert!(store.get(&runs[1]).await.is_ok(), "the LIVE run was reaped");
    assert_eq!(
        enumerate(store.as_ref(), one())
            .await
            .unwrap()
            .records
            .len(),
        2
    );

    // ⚠️ The graveyard must SHRINK, and a mutation sweep found nothing asserting it: writing
    // the head back with the entries intact deletes the objects and keeps naming them, so
    // every later reap re-deletes the same absent keys and reports work it did not do. The
    // record never converges and stays pinned at its bound.
    let (head, _) = read_head(store.as_ref(), 0).await.unwrap();
    assert!(
        head.graveyard.is_empty(),
        "reap deleted the runs and left the head still naming them: {:?}",
        head.graveyard
    );
    assert_eq!(
        reap(store.as_ref(), 0, 0).await.unwrap(),
        0,
        "a second reap found work to do, so the first did not record what it deleted"
    );
}

#[tokio::test]
async fn the_retention_window_is_counted_in_folds() {
    // ⚠️ `retention = 2` keeps two superseded runs, so run 0 survives the next two folds and
    // goes on the third. Spelled out because this is exactly the arithmetic an off-by-one
    // hides in, and getting it wrong reaps a run a reader is on.
    let store = Arc::new(MemoryStore::new());
    let runs = folds(&store, 3).await;
    reap(store.as_ref(), 0, 2).await.unwrap();
    assert!(
        store.get(&runs[0]).await.is_ok(),
        "run 0 was reaped with two further folds of retention promised"
    );

    let more = folds(&store, 1).await;
    reap(store.as_ref(), 0, 2).await.unwrap();
    assert!(
        store.get(&runs[0]).await.is_err(),
        "run 0 outlived its window"
    );
    assert!(
        store.get(&runs[2]).await.is_ok(),
        "run 2 was inside the window"
    );
    assert!(store.get(&more[0]).await.is_ok(), "the live run was reaped");
}

#[tokio::test]
async fn a_reap_that_loses_the_head_cas_still_deleted_and_is_idempotent() {
    // ⚠️ The ordering no functional test can see: on a store that never fails, both orders
    // reap the right objects. Delete-then-CAS leaves entries naming absent objects, which a
    // later reap re-deletes harmlessly. CAS-then-delete leaves objects with NO record, and
    // their keys cannot be derived from anything that survives.
    let store = Arc::new(MemoryStore::new());
    let runs = folds(&store, 2).await;

    let hostile = Flaky::always_contended();
    for k in [
        pstore_catalog::head_key(0),
        runs[0].clone(),
        runs[1].clone(),
    ] {
        let bytes = store.get(&k).await.unwrap();
        hostile.put(&k, bytes).await.unwrap();
    }
    assert!(
        reap(&hostile, 0, 0).await.is_err(),
        "a reap committed against a backend refusing every CAS"
    );
    assert!(
        hostile.get(&runs[0]).await.is_err(),
        "the delete was issued after the CAS, so a lost CAS leaks the object with no record"
    );
    let (head, _) = read_head(&hostile, 0).await.unwrap();
    assert_eq!(
        head.graveyard.len(),
        1,
        "the graveyard forgot what it had already deleted"
    );

    // And the record still names an absent object, which the next reap must tolerate.
    let again = reap(store.as_ref(), 0, 0).await.unwrap();
    assert_eq!(again, 1);
}

#[tokio::test]
async fn reaping_does_not_list() {
    let t = TenantId(0);
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let view = Arc::new(acct.as_tenant(t));
    folds(&view, 2).await;
    reap(view.as_ref(), 0, 0).await.unwrap();
    enumerate(view.as_ref(), one()).await.unwrap();
    assert_eq!(acct.count(t, OpClass::List), 0, "the reap path listed");
}

#[tokio::test]
async fn an_append_between_a_fold_and_a_reap_keeps_the_graveyard() {
    // ⚠️ `Appender::record` writes the head too, and it keeps the graveyard only *because* it
    // mutates the head it read rather than constructing a fresh one. A refactor to
    // `BucketHead { run_epoch, digest, pending: next }` would erase the record and orphan
    // every run in it -- permanently, since the keys are derived.
    let store = Arc::new(MemoryStore::new());
    let runs = folds(&store, 2).await;

    let a = Appender::new(Arc::clone(&store), one());
    a.record(&rec(900, 9)).await.unwrap();
    let (head, _) = read_head(store.as_ref(), 0).await.unwrap();
    assert_eq!(
        head.graveyard.len(),
        1,
        "an append erased the graveyard, orphaning every run it named"
    );

    assert_eq!(reap(store.as_ref(), 0, 0).await.unwrap(), 1);
    assert!(store.get(&runs[0]).await.is_err());
}

#[tokio::test]
async fn a_retention_wider_than_the_graveyard_is_refused() {
    // ⚠️ A promise larger than the record. With a window of `MAX_GRAVEYARD + 1` against a
    // record of `MAX_GRAVEYARD`, the oldest entry is evicted from the graveyard while it is
    // still inside the window it was promised -- and then it is unreachable forever.
    let store = Arc::new(MemoryStore::new());
    let runs = folds(&store, 2).await;
    let err = reap(store.as_ref(), 0, MAX_GRAVEYARD + 1)
        .await
        .expect_err("a window wider than the record was accepted");
    assert!(matches!(err, CatalogError::Corrupt(_)), "{err}");
    assert!(
        store.get(&runs[0]).await.is_ok(),
        "a refused reap deleted anyway"
    );

    // ⚠️ And the boundary itself is ACCEPTED. Refusing `MAX_GRAVEYARD` too would reject the
    // widest window the record can actually keep — a usable knob turned off. The sweep found
    // `>` and `>=` indistinguishable until this line existed.
    assert_eq!(
        reap(store.as_ref(), 0, MAX_GRAVEYARD).await.unwrap(),
        0,
        "the widest window the record can keep was refused"
    );
    assert!(store.get(&runs[0]).await.is_ok());
}

#[tokio::test]
async fn a_head_written_before_the_graveyard_decodes_and_reaps_nothing() {
    // Forward compatibility that cannot be constructed cannot be tested, and heads written
    // before this milestone outlive every reader that meets them.
    let store = Arc::new(MemoryStore::new());
    folds(&store, 2).await;
    let (head, _) = read_head(store.as_ref(), 0).await.unwrap();
    let old = BucketHead {
        graveyard: Vec::new(),
        ..head.clone()
    };
    store
        .put(&pstore_catalog::head_key(0), old.encode_for_test().into())
        .await
        .unwrap();

    let (back, _) = read_head(store.as_ref(), 0).await.unwrap();
    assert!(back.graveyard.is_empty());
    assert_eq!(
        reap(store.as_ref(), 0, 0).await.unwrap(),
        0,
        "a head with no graveyard reaped something"
    );
    assert_eq!(
        enumerate(store.as_ref(), one())
            .await
            .unwrap()
            .records
            .len(),
        2
    );
}

#[tokio::test]
async fn the_graveyard_is_bounded_by_max_graveyard() {
    // ⚠️ The head is CAS'd on EVERY append and fold. An unbounded graveyard grows the hottest
    // small object in the catalog forever, which is the failure `MAX_PENDING` exists to
    // prevent one field over.
    let store = Arc::new(MemoryStore::new());
    folds(&store, (MAX_GRAVEYARD as u64) + 4).await;
    let (head, _) = read_head(store.as_ref(), 0).await.unwrap();
    assert!(
        head.graveyard.len() <= MAX_GRAVEYARD,
        "the graveyard holds {} entries against a bound of {MAX_GRAVEYARD}",
        head.graveyard.len()
    );
}

#[tokio::test]
async fn a_width_change_that_loses_its_race_is_refused_by_name() {
    // ⚠️ Not part of M6d's delta, and covered here because the reap work put the crate's
    // region floor in reach and this was the gap. `write_root` is how a deployment changes
    // its bucket count, and its `previous: None` arm — the first-ever root — had never been
    // reached on a store that gets past `require_fencing`.
    //
    // A lost race on the root is not a retry: two nodes disagreeing about the width would
    // name different buckets, so the loser must be told rather than left to re-derive.
    let store = MemoryStore::new();
    pstore_catalog::write_root(
        &store,
        pstore_catalog::Root {
            epoch: Epoch(1),
            width: one(),
        },
        None,
    )
    .await
    .expect("the first root write is a create-if-absent");
    let (back, _) = pstore_catalog::read_root(&store).await.unwrap();
    assert_eq!(back.width, one());

    // A second create-if-absent against an object that now exists is a lost race.
    let err = pstore_catalog::write_root(
        &store,
        pstore_catalog::Root {
            epoch: Epoch(2),
            width: Width::new(4).expect("width 4"),
        },
        None,
    )
    .await
    .expect_err("a second create-if-absent must not overwrite the root");
    assert!(matches!(err, CatalogError::RootContended), "{err}");
    assert_eq!(
        pstore_catalog::read_root(&store).await.unwrap().0.width,
        one(),
        "the refused write changed the deployment's width anyway"
    );
}

#[tokio::test]
async fn a_width_change_is_conditioned_on_the_root_it_read() {
    // ⚠️ The gap the region floor found. `read_root` returned no tag, so `write_root`'s
    // `Some(previous)` arm had no reachable caller: every caller could pass only `None`,
    // which is create-if-absent and fails once the root exists. The width could be set once
    // and **never changed** — a concrete blocker under OQ-8, one layer below its protocol
    // question.
    let store = MemoryStore::new();
    let first = pstore_catalog::Root {
        epoch: Epoch(1),
        width: one(),
    };
    pstore_catalog::write_root(&store, first, None)
        .await
        .unwrap();

    let (root, tag) = pstore_catalog::read_root(&store).await.unwrap();
    assert_eq!(root, first);
    let tag = tag.expect("a root that exists has a tag");
    pstore_catalog::write_root(
        &store,
        pstore_catalog::Root {
            epoch: Epoch(2),
            width: Width::new(4).expect("width 4"),
        },
        Some(tag.clone()),
    )
    .await
    .expect("a width change conditioned on the root it read must land");
    let (after, _) = pstore_catalog::read_root(&store).await.unwrap();
    assert_eq!(after.width, Width::new(4).expect("width 4"));

    // ⚠️ And the stale tag loses rather than overwriting. Two nodes disagreeing about the
    // width name different buckets, so the loser must be told.
    let err = pstore_catalog::write_root(&store, first, Some(tag))
        .await
        .expect_err("a stale root tag must not win");
    assert!(matches!(err, CatalogError::RootContended), "{err}");
}

#[tokio::test]
async fn a_root_write_that_fails_for_io_is_not_reported_as_a_lost_race() {
    // ⚠️ The distinction is what a caller does next. `RootContended` says someone else won,
    // so re-read and decide again; an I/O failure says we do not know, and re-deriving a
    // width from a root we could not write is how two nodes end up naming different buckets.
    let store = Flaky::refusing(&[0]);
    let err = pstore_catalog::write_root(
        &store,
        pstore_catalog::Root {
            epoch: Epoch(1),
            width: one(),
        },
        None,
    )
    .await
    .expect_err("an injected write failure must surface");
    assert!(
        !matches!(err, CatalogError::RootContended),
        "an I/O failure was reported as a lost race: {err}"
    );
}
