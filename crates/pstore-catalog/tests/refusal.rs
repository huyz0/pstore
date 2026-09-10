//! What the catalog does when the store says no.
//!
//! ⚠️ These are not here to touch lines. Every one of them is a way for the catalog to be
//! **silently wrong** rather than loudly broken — an enumeration that under-reports, a
//! pointer to a run that is gone, a retry loop with no exit — and each is invisible to every
//! request count, depth measurement and functional assertion in the other files.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::{BlobStore, MemoryStore};
use pstore_catalog::{
    Appender, CatalogError, TenantRecord, Width, enumerate, fold, read_head, read_root,
};
use pstore_testkit::flaky::Flaky;
use pstore_types::{Epoch, TenantId};
use std::sync::Arc;

fn one() -> Width {
    Width::new(1).expect("one bucket")
}

async fn seeded() -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::new());
    let app = Appender::new(Arc::clone(&store), one());
    app.observe(&TenantRecord::live(TenantId(1), Epoch(1), &[]))
        .await
        .unwrap();
    fold(store.as_ref(), 0).await.unwrap();
    store
}

#[tokio::test]
async fn a_contended_bucket_gives_up_rather_than_spinning() {
    // ⚠️ `Contended` means "retry unchanged", so the loop's exit condition is the backend
    // eventually saying something else. A backend that never does — hostile, throttling, or
    // broken — turns this into a spin on a path a caller is awaiting. Bounded, not `loop`.
    let store = Arc::new(Flaky::always_contended());
    let app = Appender::new(Arc::clone(&store), one());
    let err = app
        .record(&TenantRecord::live(TenantId(1), Epoch(1), &[]))
        .await
        .expect_err("a permanently contended backend must not be retried forever");
    assert!(matches!(err, CatalogError::Contended(0, _)), "{err}");
}

#[tokio::test]
async fn a_fold_on_a_contended_bucket_gives_up_too() {
    let store = Arc::new(MemoryStore::new());
    let app = Appender::new(Arc::clone(&store), one());
    app.record(&TenantRecord::live(TenantId(1), Epoch(1), &[]))
        .await
        .unwrap();
    // Copy the pending state into a store that will never let a CAS land.
    let hostile = Flaky::always_contended();
    let (bytes, _) = store
        .get_with_tag(&pstore_catalog::head_key(0))
        .await
        .unwrap();
    hostile
        .put(&pstore_catalog::head_key(0), bytes)
        .await
        .unwrap();

    let err = fold(&hostile, 0)
        .await
        .expect_err("a fold must not spin on a contended pointer");
    assert!(matches!(err, CatalogError::Contended(0, _)), "{err}");
}

#[tokio::test]
async fn a_refused_write_is_an_error_not_a_silent_drop() {
    // A store that refuses every conditional write. `observe` must report that, because it
    // returns `true` for "recorded" and a caller trusts that answer.
    let store = Arc::new(Flaky::new(1, 1.0));
    let app = Appender::new(Arc::clone(&store), one());
    assert!(
        app.observe(&TenantRecord::live(TenantId(1), Epoch(1), &[]))
            .await
            .is_err()
    );
    // And it did not remember the record it failed to write: the next attempt tries again.
    assert!(
        app.observe(&TenantRecord::live(TenantId(1), Epoch(1), &[]))
            .await
            .is_err()
    );
}

/// A store holding one pending record, whose write at `at` will be refused.
///
/// Ordinal 0 is the `put` that seeds the pointer, so a fold's run PUT is 1 and its pointer
/// CAS is 2.
async fn primed(at: u64) -> Flaky {
    let seed = Arc::new(MemoryStore::new());
    Appender::new(Arc::clone(&seed), one())
        .record(&TenantRecord::live(TenantId(1), Epoch(1), &[]))
        .await
        .unwrap();
    let (bytes, _) = seed
        .get_with_tag(&pstore_catalog::head_key(0))
        .await
        .unwrap();
    let store = Flaky::refusing(&[at]);
    store
        .put(&pstore_catalog::head_key(0), bytes)
        .await
        .unwrap();
    store
}

#[tokio::test]
async fn a_refused_run_write_leaves_the_pointer_alone() {
    let store = primed(1).await;
    assert!(fold(&store, 0).await.is_err());
    assert_eq!(store.failures(), 1);
    // The pointer still names no run, and the record is still pending, so nothing was lost.
    let out = enumerate(&store, one()).await.unwrap();
    assert_eq!(out.records.len(), 1);
    let (head, _) = read_head(&store, 0).await.unwrap();
    assert_eq!(head.run_epoch, Epoch::ZERO);
}

#[tokio::test]
async fn a_refused_pointer_cas_leaves_an_orphan_run_and_no_loss() {
    // ⚠️ The half-done fold. The run landed and the pointer did not, so the run is garbage
    // -- which is the *safe* half of the ordering. The other order leaves a pointer naming
    // an object that was never written, which is a bucket lost.
    let store = primed(2).await;
    assert!(fold(&store, 0).await.is_err());
    assert_eq!(store.failures(), 1);
    let out = enumerate(&store, one()).await.unwrap();
    assert_eq!(out.records.len(), 1);
}

#[tokio::test]
async fn a_pointer_to_a_run_that_is_gone_is_an_error() {
    // ⚠️ The difference this test is about: a derived pointer key that 404s means "empty
    // bucket", and a run key that 404s means a bucket's worth of tenants has vanished.
    // Treating the second like the first is a catalog that reports fewer tenants than exist
    // and returns `Ok`.
    let store = seeded().await;
    let (head, _) = read_head(store.as_ref(), 0).await.unwrap();
    let run = head.run(0).expect("a run was folded");
    store.delete_batch(&[run]).await.unwrap();

    let err = enumerate(store.as_ref(), one())
        .await
        .expect_err("a missing run must not read as an empty bucket");
    assert!(matches!(err, CatalogError::MissingRun(0)), "{err}");
}

#[tokio::test]
async fn a_refused_run_read_is_an_error() {
    let w = one();
    // Seeding is 1 observe (1 read) + 1 fold (1 head read + 1 run read is skipped on the
    // first fold, so 1). Enumeration then reads the pointer (ordinal 2) and the run (3).
    let store = Arc::new(Flaky::refusing_reads_at(&[3]));
    let app = Appender::new(Arc::clone(&store), w);
    app.observe(&TenantRecord::live(TenantId(1), Epoch(1), &[]))
        .await
        .unwrap();
    fold(store.as_ref(), 0).await.unwrap();

    assert!(enumerate(store.as_ref(), w).await.is_err());
    assert_eq!(store.failures(), 1, "no read was actually refused");
}

#[tokio::test]
async fn a_refused_root_read_is_an_error_not_the_default_width() {
    // Absence means "never widened" and yields the default. A *refusal* must not, or a
    // reader on a widened deployment silently falls back to the wrong bucket set.
    let store = Flaky::refusing_reads();
    assert!(read_root(&store).await.is_err());
}

#[tokio::test]
async fn a_catalog_write_on_a_divergent_backend_is_refused() {
    // ⚠️ M7a criterion 8. The sentence that argues `pstore-cluster`'s roster CAS out of the
    // guard — cluster state re-converges by gossip — argues the opposite way here: the
    // catalog is tenant state, and the create-if-absent on a fresh bucket pointer is exactly
    // the primitive MinIO accepts and ignores.
    let acc = pstore_blob::Accounted::new(pstore_testkit::claims::Claims::divergent_cas(
        "wildcard ignored",
    ));
    let bill = TenantId(0);
    let store = Arc::new(acc.as_tenant(bill));
    let app = Appender::new(Arc::clone(&store), one());

    for e in [
        app.record(&TenantRecord::live(TenantId(1), Epoch(1), &[]))
            .await
            .expect_err("record must refuse"),
        app.observe(&TenantRecord::live(TenantId(1), Epoch(1), &[]))
            .await
            .expect_err("observe must refuse"),
        fold(store.as_ref(), 0).await.expect_err("fold must refuse"),
        // ⚠️ M6d, and the reason bites harder here than at any other door: a reaper on a
        // backend that cannot fence deletes objects and then fails to record that it did,
        // which is exactly the unreachable garbage the graveyard exists to prevent.
        pstore_catalog::reap(store.as_ref(), 0, 0)
            .await
            .expect_err("reap must refuse"),
        pstore_catalog::write_root(
            store.as_ref(),
            pstore_catalog::Root {
                epoch: Epoch(1),
                width: one(),
            },
            None,
        )
        .await
        .expect_err("write_root must refuse"),
    ] {
        let msg = e.to_string();
        assert!(
            matches!(e, CatalogError::BackendCannotFence { .. }),
            "expected a fencing refusal, got {msg}"
        );
        assert!(msg.contains("compare_and_swap"), "{msg}");
    }

    for class in [
        pstore_blob::OpClass::Read,
        pstore_blob::OpClass::Write,
        pstore_blob::OpClass::Delete,
        pstore_blob::OpClass::List,
    ] {
        assert_eq!(acc.total(class), 0, "{class:?} issued by a refused write");
    }
}
