//! Criteria 6, 9 and 11: what a fold publishes, and what it must not lose.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::MemoryStore;
use pstore_catalog::{Appender, TenantRecord, Width, enumerate, fold, read_head};
use pstore_types::{Epoch, TenantId};
use std::sync::Arc;

fn one() -> Width {
    Width::new(1).expect("one bucket")
}

fn idx(n: &str) -> Vec<String> {
    vec![n.to_owned()]
}

async fn tenants(store: &MemoryStore) -> Vec<u128> {
    enumerate(store, one())
        .await
        .unwrap()
        .records
        .iter()
        .map(|r| r.tenant.0)
        .collect()
}

#[tokio::test]
async fn a_pending_record_is_visible() {
    let store = Arc::new(MemoryStore::new());
    let app = Appender::new(Arc::clone(&store), one());
    app.observe(&TenantRecord::live(TenantId(7), Epoch(1), &idx("a")))
        .await
        .unwrap();

    // Nothing has been folded, so the run does not exist. Reading only the run would report
    // an empty catalog and every request-count assertion would still pass.
    let (head, _) = read_head(store.as_ref(), 0).await.unwrap();
    assert_eq!(head.run_epoch, Epoch::ZERO);
    assert_eq!(tenants(&store).await, vec![7]);
}

#[tokio::test]
async fn a_folded_record_appears_once() {
    let store = Arc::new(MemoryStore::new());
    let app = Appender::new(Arc::clone(&store), one());
    app.observe(&TenantRecord::live(TenantId(7), Epoch(1), &idx("a")))
        .await
        .unwrap();
    assert!(fold(store.as_ref(), 0).await.unwrap());

    assert_eq!(tenants(&store).await, vec![7]);

    // ⚠️ Now the case that matters: the tenant is in the run **and** back in `pending`. A
    // reader that returns run + pending without letting one supersede the other lists tenant
    // 7 twice here -- and "the catalog lists a tenant twice" is not something a request
    // count or a depth measurement can see.
    app.observe(&TenantRecord::live(TenantId(7), Epoch(2), &idx("b")))
        .await
        .unwrap();
    let out = enumerate(store.as_ref(), one()).await.unwrap();
    assert_eq!(out.records.len(), 1);
    assert_eq!(out.records[0].indexes, idx("b"));

    // A fold with nothing pending has nothing to do and says so, rather than publishing an
    // empty epoch and churning the pointer.
    fold(store.as_ref(), 0).await.unwrap();
    assert!(!fold(store.as_ref(), 0).await.unwrap());
}

#[tokio::test]
async fn a_tombstone_hides_a_tenant_without_being_dropped() {
    let store = Arc::new(MemoryStore::new());
    let app = Appender::new(Arc::clone(&store), one());
    app.observe(&TenantRecord::live(TenantId(7), Epoch(1), &idx("a")))
        .await
        .unwrap();
    app.observe(&TenantRecord::live(TenantId(8), Epoch(1), &idx("a")))
        .await
        .unwrap();
    fold(store.as_ref(), 0).await.unwrap();

    app.observe(&TenantRecord::deleted(TenantId(7), Epoch(2)))
        .await
        .unwrap();
    assert_eq!(tenants(&store).await, vec![8]);

    fold(store.as_ref(), 0).await.unwrap();
    let (head, _) = read_head(store.as_ref(), 0).await.unwrap();
    assert!(head.pending.is_empty());
    assert_eq!(tenants(&store).await, vec![8]);

    // ⚠️ The load-bearing half, and it needs a stale writer to show. A fold that filters
    // tombstones out as a tidy-up leaves a run with no record of tenant 7 at all — so the
    // next appender still holding an old view reinserts it and the tenant comes back from
    // the dead. The tombstone is what makes that arrival lose on epoch.
    app.record(&TenantRecord::live(TenantId(7), Epoch(1), &idx("a")))
        .await
        .unwrap();
    assert_eq!(
        tenants(&store).await,
        vec![8],
        "the deleted tenant came back"
    );
    fold(store.as_ref(), 0).await.unwrap();
    assert_eq!(tenants(&store).await, vec![8]);
}

#[tokio::test]
async fn an_older_epoch_does_not_overwrite_a_newer_pending_one() {
    // ⚠️ The run is not the only place two records for one tenant meet. Both can be
    // **pending at once** — a second appender starts with an empty change-check map, so a
    // node holding a stale view records unconditionally — and the pointer is the one place
    // the epoch rule has to hold before a fold ever runs.
    let store = Arc::new(MemoryStore::new());
    let app = Appender::new(Arc::clone(&store), one());
    app.record(&TenantRecord::live(TenantId(7), Epoch(9), &idx("new")))
        .await
        .unwrap();
    app.record(&TenantRecord::live(TenantId(7), Epoch(2), &idx("old")))
        .await
        .unwrap();

    let recs = enumerate(store.as_ref(), one()).await.unwrap().records;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].epoch, Epoch(9));
    assert_eq!(recs[0].indexes, idx("new"));
}

#[tokio::test]
async fn a_stale_live_record_does_not_resurrect_a_pending_tombstone() {
    // The same hole, in the shape that matters most: the tombstone has not been folded yet,
    // so nothing in the run is holding the deletion down.
    let store = Arc::new(MemoryStore::new());
    let app = Appender::new(Arc::clone(&store), one());
    app.record(&TenantRecord::live(TenantId(7), Epoch(1), &idx("a")))
        .await
        .unwrap();
    fold(store.as_ref(), 0).await.unwrap();
    app.record(&TenantRecord::deleted(TenantId(7), Epoch(5)))
        .await
        .unwrap();
    app.record(&TenantRecord::live(TenantId(7), Epoch(1), &idx("a")))
        .await
        .unwrap();

    assert_eq!(
        tenants(&store).await,
        Vec::<u128>::new(),
        "the tenant came back"
    );
    fold(store.as_ref(), 0).await.unwrap();
    assert_eq!(tenants(&store).await, Vec::<u128>::new());
}

#[tokio::test]
async fn an_older_epoch_does_not_overwrite_a_newer_one() {
    let store = Arc::new(MemoryStore::new());
    let app = Appender::new(Arc::clone(&store), one());
    app.record(&TenantRecord::live(TenantId(7), Epoch(9), &idx("new")))
        .await
        .unwrap();
    fold(store.as_ref(), 0).await.unwrap();
    // A node that was paused and woke up holding a stale view.
    app.record(&TenantRecord::live(TenantId(7), Epoch(2), &idx("old")))
        .await
        .unwrap();
    fold(store.as_ref(), 0).await.unwrap();

    let recs = enumerate(store.as_ref(), one()).await.unwrap().records;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].epoch, Epoch(9));
    assert_eq!(recs[0].indexes, idx("new"));
}
