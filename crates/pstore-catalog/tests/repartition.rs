//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Putting every record in the bucket the current width names — M6h.
//!
//! ⚠️ [M6g](../../../docs/milestones/M6g/SPEC.md) deferred this as "pruning" and called it
//! blocked. Most of the blocker was already gone: a stale-width **reader** is refused by the
//! census check, and `bucket_of` has exactly two shipped callers, so nothing looks a tenant up
//! by derived key. What remained is the writer — a stale-width appender puts a record in a
//! bucket the current width does not own, and that copy may be the **only** one, or the
//! **newest**. So a record may leave a bucket only when its owner holds a copy at least as
//! new; otherwise it is *moved*. That is why this is a re-partition and not a prune.

use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_catalog::{
    Appender, CatalogError, TenantRecord, Width, bucket_of, enumerate, fold, read_head, read_root,
    read_run_for_test, repartition, split, write_root,
};
use pstore_types::{Epoch, TenantId};
use std::sync::Arc;

const TENANTS: u128 = 200;

fn rec(t: u128, e: u64) -> TenantRecord {
    TenantRecord::live(TenantId(t), Epoch(e), &["idx".to_owned()])
}

fn w(n: u32) -> Width {
    Width::new(n).expect("a width")
}

async fn seeded<S: BlobStore>(store: &Arc<S>, n: u32) -> Width {
    let width = w(n);
    write_root(
        store.as_ref(),
        pstore_catalog::Root {
            epoch: Epoch(1),
            width,
        },
        None,
    )
    .await
    .unwrap();
    let a = Appender::new(Arc::clone(store), width);
    for t in 0..TENANTS {
        a.record(&rec(t, 1)).await.unwrap();
    }
    for b in width.all() {
        fold(store.as_ref(), b).await.unwrap();
    }
    width
}

/// Every record held anywhere, with the bucket holding it.
async fn held<S: BlobStore>(store: &S, width: Width) -> Vec<(u32, TenantRecord)> {
    let mut out = Vec::new();
    for b in width.all() {
        let (head, _) = read_head(store, b).await.unwrap();
        for r in read_run_for_test(store, b, &head)
            .await
            .unwrap()
            .into_iter()
            .chain(head.pending.iter().cloned())
        {
            out.push((b, r));
        }
    }
    out
}

#[tokio::test]
async fn every_bucket_owns_what_it_holds_after_a_repartition() {
    let store = Arc::new(MemoryStore::new());
    seeded(&store, 8).await;
    let after = split(store.as_ref()).await.unwrap();
    repartition(store.as_ref()).await.unwrap();

    for (b, r) in held(store.as_ref(), after).await {
        assert_eq!(
            bucket_of(r.tenant, after),
            b,
            "bucket {b} holds tenant {} which width {after:?} puts elsewhere",
            r.tenant.0
        );
    }
}

#[tokio::test]
async fn a_repartition_loses_no_tenant() {
    let store = Arc::new(MemoryStore::new());
    seeded(&store, 8).await;
    let after = split(store.as_ref()).await.unwrap();
    let mut before: Vec<u128> = enumerate(store.as_ref(), after)
        .await
        .unwrap()
        .records
        .iter()
        .map(|r| r.tenant.0)
        .collect();
    before.sort_unstable();

    repartition(store.as_ref()).await.unwrap();
    let mut got: Vec<u128> = enumerate(store.as_ref(), after)
        .await
        .unwrap()
        .records
        .iter()
        .map(|r| r.tenant.0)
        .collect();
    got.sort_unstable();
    assert_eq!(got, before, "the re-partition lost or duplicated a tenant");
}

#[tokio::test]
async fn a_stale_width_writers_record_is_moved_not_dropped() {
    // ⚠️ The criterion that makes this a re-partition. A prune — "delete what this bucket does
    // not own" — deletes the ONLY copy of a live record.
    let store = Arc::new(MemoryStore::new());
    let before = seeded(&store, 8).await;
    let stale = Appender::new(Arc::clone(&store), before);
    let after = split(store.as_ref()).await.unwrap();

    // ⚠️ A tenant the doubling actually moves — `h % 2w` equals `h % w` for half of them, so a
    // hardcoded id is a coin flip. The first fixture used one that did not move and asserted
    // nothing.
    let late = (9_000u128..)
        .map(TenantId)
        .find(|t| bucket_of(*t, before) != bucket_of(*t, after))
        .expect("a tenant the doubling moves");
    stale.record(&rec(late.0, 5)).await.unwrap();
    let owns = bucket_of(late, after);

    let moved = repartition(store.as_ref()).await.unwrap();
    assert!(moved > 0);
    assert!(
        enumerate(store.as_ref(), after)
            .await
            .unwrap()
            .records
            .iter()
            .any(|r| r.tenant == late),
        "the only copy of a live record was deleted rather than moved"
    );
    let (head, _) = read_head(store.as_ref(), owns).await.unwrap();
    assert!(
        read_run_for_test(store.as_ref(), owns, &head)
            .await
            .unwrap()
            .iter()
            .any(|r| r.tenant == late),
        "the record survived but is still not in the bucket that owns it"
    );
}

#[tokio::test]
async fn an_older_copy_does_not_come_back() {
    // ⚠️ Two copies at different epochs, the older in the wrong bucket. Merging the wrong way
    // resurrects a superseded record — and since a tombstone is just a newer record, that
    // would undo a delete.
    let store = Arc::new(MemoryStore::new());
    let before = seeded(&store, 8).await;
    let stale = Appender::new(Arc::clone(&store), before);
    let after = split(store.as_ref()).await.unwrap();

    // Tenant 3 exists at epoch 1 wherever the split put it. Write an OLDER-epoch copy through
    // the stale writer, then a delete at the current width.
    let t = TenantId(3);
    stale
        .record(&TenantRecord::live(t, Epoch(0), &[]))
        .await
        .unwrap();
    let fresh = Appender::new(Arc::clone(&store), after);
    fresh
        .record(&TenantRecord::deleted(t, Epoch(9)))
        .await
        .unwrap();

    repartition(store.as_ref()).await.unwrap();
    assert!(
        !enumerate(store.as_ref(), after)
            .await
            .unwrap()
            .records
            .iter()
            .any(|r| r.tenant == t),
        "a re-partition resurrected a deleted tenant from an older copy"
    );
}

#[tokio::test]
async fn a_second_repartition_moves_nothing() {
    let store = Arc::new(MemoryStore::new());
    seeded(&store, 8).await;
    split(store.as_ref()).await.unwrap();
    assert!(repartition(store.as_ref()).await.unwrap() > 0);

    let (root, _) = read_root(store.as_ref()).await.unwrap();
    let snapshot = held(store.as_ref(), root.width).await;
    assert_eq!(
        repartition(store.as_ref()).await.unwrap(),
        0,
        "a settled catalog was re-partitioned again"
    );
    assert_eq!(
        held(store.as_ref(), root.width).await,
        snapshot,
        "a no-op re-partition rewrote a bucket"
    );
}

#[tokio::test]
async fn a_repartition_reclaims_the_splits_duplicates() {
    // ⚠️ Pulls the opposite way from `a_stale_width_writers_record_is_moved_not_dropped`, and
    // both must hold: deleting everything a bucket does not own satisfies this one and breaks
    // that one.
    let store = Arc::new(MemoryStore::new());
    seeded(&store, 8).await;
    let after = split(store.as_ref()).await.unwrap();
    let duplicated = held(store.as_ref(), after).await.len();
    assert!(
        duplicated > TENANTS as usize,
        "the split did not duplicate anything, so there is nothing to reclaim"
    );

    repartition(store.as_ref()).await.unwrap();
    assert_eq!(
        held(store.as_ref(), after).await.len(),
        TENANTS as usize,
        "the split's duplicates are still held"
    );
}

#[tokio::test]
async fn repartitioning_does_not_list() {
    let t = TenantId(0);
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let view = Arc::new(acct.as_tenant(t));
    seeded(&view, 8).await;
    split(view.as_ref()).await.unwrap();
    repartition(view.as_ref()).await.unwrap();
    assert_eq!(acct.count(t, OpClass::List), 0, "the re-partition listed");
}

#[tokio::test]
async fn a_repartition_on_a_divergent_backend_is_refused() {
    let acc = Accounted::new(pstore_testkit::claims::Claims::divergent_cas(
        "wildcard ignored",
    ));
    let bill = TenantId(0);
    let store = Arc::new(acc.as_tenant(bill));
    let err = repartition(store.as_ref())
        .await
        .expect_err("a re-partition ran against a backend that cannot fence");
    assert!(
        matches!(err, CatalogError::BackendCannotFence { .. }),
        "{err}"
    );
    assert_eq!(acc.count(bill, OpClass::Write), 0);
}
