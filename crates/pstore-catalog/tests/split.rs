//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Doubling the catalog's width — M6g, and OQ-8.
//!
//! ⚠️ **`h % 2w` is either `h % w` or `h % w + w`**, so bucket `b` splits into exactly `b` and
//! `b + w` and no record ever moves between two old buckets. That is what makes a split `w`
//! independent partitions rather than a reshuffle.
//!
//! ⚠️ **Pruning is the only step that can make a reader wrong**, and it is not built. Until the
//! old buckets are pruned, an old-width reader reading `0..w` still finds every tenant and a
//! stale-width *writer*'s record still lands somewhere a new-width reader looks. Doing it
//! safely needs a bound on how long a stale-width process may live, and nothing provides one.

use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_catalog::{
    Appender, CatalogError, DEFAULT_WIDTH, MAX_WIDTH, TenantRecord, Width, bucket_of, enumerate,
    fold, read_head, read_root, split, write_root,
};
use pstore_types::{Epoch, TenantId};
use std::sync::Arc;

const TENANTS: u128 = 400;

fn rec(t: u128) -> TenantRecord {
    TenantRecord::live(TenantId(t), Epoch(1), &["idx".to_owned()])
}

fn w(n: u32) -> Width {
    Width::new(n).expect("a width")
}

/// A catalog of `TENANTS` tenants at width `n`, folded so every bucket has a run.
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
        a.record(&rec(t)).await.unwrap();
    }
    for b in width.all() {
        fold(store.as_ref(), b).await.unwrap();
    }
    width
}

#[tokio::test]
async fn every_tenant_survives_a_split() {
    let store = Arc::new(MemoryStore::new());
    let before = seeded(&store, 8).await;
    let mut want: Vec<u128> = enumerate(store.as_ref(), before)
        .await
        .unwrap()
        .records
        .iter()
        .map(|r| r.tenant.0)
        .collect();
    want.sort_unstable();
    assert_eq!(want.len(), TENANTS as usize);

    let after = split(store.as_ref()).await.unwrap();
    assert_eq!(after, w(16));
    let mut got: Vec<u128> = enumerate(store.as_ref(), after)
        .await
        .unwrap()
        .records
        .iter()
        .map(|r| r.tenant.0)
        .collect();
    got.sort_unstable();
    assert_eq!(got, want, "the split lost or duplicated tenants");
}

#[tokio::test]
async fn a_record_lands_where_the_new_width_says() {
    // ⚠️ Read the bucket DIRECTLY. Enumerating reads every bucket, so it passes whether or not
    // anything moved — the partition predicate could be inverted and the census would agree.
    let store = Arc::new(MemoryStore::new());
    seeded(&store, 8).await;
    let after = split(store.as_ref()).await.unwrap();

    let mut moved = 0usize;
    for t in 0..TENANTS {
        let b = bucket_of(TenantId(t), after);
        let (head, _) = read_head(store.as_ref(), b).await.unwrap();
        let run = pstore_catalog::read_run_for_test(store.as_ref(), b, &head)
            .await
            .unwrap();
        let here = run
            .iter()
            .chain(head.pending.iter())
            .any(|r| r.tenant.0 == t);
        assert!(
            here,
            "tenant {t} is not in bucket {b}, where width {after:?} puts it"
        );
        if b >= 8 {
            moved += 1;
        }
    }
    assert!(
        moved > 0 && moved < TENANTS as usize,
        "the split moved {moved} of {TENANTS} tenants, so the partition is not a partition"
    );
}

#[tokio::test]
async fn the_old_buckets_still_hold_every_record() {
    // ⚠️ The "stale rather than wrong" claim, and the property pruning would break. Asserted
    // against the buckets rather than through `enumerate`, which refuses once the root has
    // moved — the two are about different things.
    let store = Arc::new(MemoryStore::new());
    let before = seeded(&store, 8).await;
    split(store.as_ref()).await.unwrap();

    for t in 0..TENANTS {
        let b = bucket_of(TenantId(t), before);
        let (head, _) = read_head(store.as_ref(), b).await.unwrap();
        let run = pstore_catalog::read_run_for_test(store.as_ref(), b, &head)
            .await
            .unwrap();
        assert!(
            run.iter()
                .chain(head.pending.iter())
                .any(|r| r.tenant.0 == t),
            "tenant {t} left old bucket {b}, so an old-width reader is now WRONG rather than \
             stale"
        );
    }
}

#[tokio::test]
async fn a_census_at_a_width_the_deployment_left_is_refused() {
    // ⚠️ Both shapes of the same failure: a split landing during the census, and a caller
    // whose width was already behind when it started. Either way some buckets were read
    // against a shape that is not the deployment's.
    //
    // ⚠️ Refusing a deliberately-behind reader is **not** a contradiction of
    // `the_old_buckets_still_hold_every_record`. That criterion is about the buckets, which is
    // what makes an old reader recoverable rather than lost; this one is about `enumerate`,
    // which will not hand back a census it cannot vouch for. It is also what pruning will
    // need: once the old buckets go, a stale-width census is genuinely short.
    let store = Arc::new(MemoryStore::new());
    let before = seeded(&store, 8).await;
    let after = split(store.as_ref()).await.unwrap();

    let err = enumerate(store.as_ref(), before)
        .await
        .expect_err("a census at a width the deployment left was returned as if complete");
    match err {
        CatalogError::WidthMoved {
            enumerated,
            current,
        } => assert_eq!((enumerated, current), (8, 16)),
        other => panic!("{other}"),
    }
    // And at the current width it succeeds.
    assert_eq!(
        enumerate(store.as_ref(), after)
            .await
            .unwrap()
            .records
            .len(),
        TENANTS as usize
    );
}

#[tokio::test]
async fn a_stale_width_writer_is_still_found() {
    // ⚠️ Worse than a stale read: a lost WRITE. An appender holding the old width puts the
    // record in `b` when the world says `b + w`; unpruned, a new-width reader reads `0..2w`,
    // which includes `b`, so it is found.
    let store = Arc::new(MemoryStore::new());
    let before = seeded(&store, 8).await;
    let stale_writer = Appender::new(Arc::clone(&store), before);
    let after = split(store.as_ref()).await.unwrap();

    stale_writer
        .record(&TenantRecord::live(
            TenantId(9_999),
            Epoch(2),
            &["late".to_owned()],
        ))
        .await
        .unwrap();
    let out = enumerate(store.as_ref(), after).await.unwrap();
    assert!(
        out.records.iter().any(|r| r.tenant.0 == 9_999),
        "a record written at the old width was lost to a new-width reader"
    );
}

#[tokio::test]
async fn splitting_stops_at_the_key_format() {
    // ⚠️ `{bucket:04x}` holds 65,536. A fifth digit is a different key space, so the split
    // refuses rather than widening into it.
    let store = Arc::new(MemoryStore::new());
    write_root(
        store.as_ref(),
        pstore_catalog::Root {
            epoch: Epoch(1),
            width: w(MAX_WIDTH),
        },
        None,
    )
    .await
    .unwrap();
    let err = split(store.as_ref())
        .await
        .expect_err("the split widened past the key format");
    assert!(matches!(err, CatalogError::BadWidth(_)), "{err}");
    assert_eq!(
        read_root(store.as_ref()).await.unwrap().0.width,
        w(MAX_WIDTH)
    );
}

#[tokio::test]
async fn splitting_does_not_list() {
    let t = TenantId(0);
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let view = Arc::new(acct.as_tenant(t));
    seeded(&view, 8).await;
    let after = split(view.as_ref()).await.unwrap();
    enumerate(view.as_ref(), after).await.unwrap();
    assert_eq!(acct.count(t, OpClass::List), 0, "the split path listed");
}

#[tokio::test]
async fn the_default_width_doubles_into_the_format() {
    // The two doublings the current key format allows, stated as a fact about the numbers
    // rather than left to arithmetic in prose.
    assert_eq!(DEFAULT_WIDTH * 4, MAX_WIDTH);
}
