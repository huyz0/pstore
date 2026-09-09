//! Criteria 2, 3, 4, 5 and 13: what enumeration costs, and what it refuses to do quietly.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_catalog::{
    Appender, Root, TenantRecord, Width, enumerate, enumerate_since, fold, read_root, write_root,
};
use pstore_testkit::depth::DepthCounting;
use pstore_testkit::flaky::Flaky;
use pstore_types::{Epoch, TenantId};
use std::sync::Arc;

const BILL: TenantId = TenantId(0);

fn width(n: u32) -> Width {
    Width::new(n).expect("a width")
}

/// `n` tenants, every bucket folded, so a deployment's cost is its shape and not its history.
async fn seed<S: pstore_blob::BlobStore>(store: Arc<S>, w: Width, n: u128) {
    let app = Appender::new(Arc::clone(&store), w);
    for i in 0..n {
        app.observe(&TenantRecord::live(
            TenantId(i),
            Epoch(1),
            &(0..50).map(|k| format!("index-{k:03}")).collect::<Vec<_>>(),
        ))
        .await
        .unwrap();
    }
    for b in w.all() {
        fold(store.as_ref(), b).await.unwrap();
    }
}

#[tokio::test]
async fn enumeration_is_two_rounds_deep() {
    let store = Arc::new(DepthCounting::new(MemoryStore::new()));
    let w = width(8);
    seed(Arc::clone(&store), w, 40).await;

    store.reset();
    let out = enumerate(store.as_ref(), w).await.unwrap();
    assert_eq!(out.records.len(), 40);
    // ⚠️ Every pointer together, then every run together. A loop that awaits each bucket
    // before issuing the next returns the identical answer at depth `1 + width`, and no
    // functional assertion anywhere can see the difference.
    assert_eq!(store.depth(), 2, "requests: {}", store.requests());

    // Cold: the root is one round more, and it is a round even when the object is absent.
    store.reset();
    let root = read_root(store.as_ref()).await.unwrap();
    enumerate(store.as_ref(), root.width).await.unwrap();
    assert_eq!(store.depth(), 3);
}

#[tokio::test]
async fn a_reader_takes_the_width_from_the_root() {
    let store = Arc::new(MemoryStore::new());
    let w = width(4);
    write_root(
        store.as_ref(),
        Root {
            epoch: Epoch(1),
            width: w,
        },
        None,
    )
    .await
    .unwrap();
    seed(Arc::clone(&store), w, 12).await;

    // A reader that reached for `DEFAULT_WIDTH` instead would read 16,384 buckets that are
    // not where anything is, and report an empty catalog with every count still "correct".
    let root = read_root(store.as_ref()).await.unwrap();
    assert_eq!(root.width, w);
    assert_eq!(
        enumerate(store.as_ref(), root.width)
            .await
            .unwrap()
            .records
            .len(),
        12
    );
}

#[tokio::test]
async fn enumeration_requests_do_not_scale_with_tenants() {
    let w = width(16);
    let mut counts = Vec::new();
    for n in [100u128, 2_000] {
        let acc = Accounted::new(MemoryStore::new());
        let store = Arc::new(acc.as_tenant(BILL));
        seed(Arc::clone(&store), w, n).await;

        let before = acc.total(OpClass::Read);
        let out = enumerate(store.as_ref(), w).await.unwrap();
        assert_eq!(out.records.len(), n as usize);
        // Not luck: both fixtures must actually occupy every bucket, or `r` differs for a
        // reason that has nothing to do with the invariant being tested.
        assert_eq!(out.marks.len(), w.get() as usize, "a bucket came up empty");
        assert_eq!(acc.total(OpClass::List), 0);
        counts.push(acc.total(OpClass::Read) - before);
    }
    // width pointers + width runs, at 100 tenants and at 2,000 alike. A single request
    // anywhere in the read path that is per-record breaks this and nothing else notices.
    assert_eq!(counts[0], counts[1]);
    assert_eq!(counts[0], u64::from(w.get()) * 2);
}

#[tokio::test]
async fn incremental_enumeration_skips_unchanged_buckets() {
    let store = Arc::new(MemoryStore::new());
    let w = width(16);
    seed(Arc::clone(&store), w, 200).await;

    let first = enumerate(store.as_ref(), w).await.unwrap();
    assert_eq!(first.read.len(), w.get() as usize);

    // Change exactly one bucket.
    let app = Appender::new(Arc::clone(&store), w);
    let moved = pstore_catalog::bucket_of(TenantId(9_999), w);
    app.observe(&TenantRecord::live(TenantId(9_999), Epoch(1), &[]))
        .await
        .unwrap();
    fold(store.as_ref(), moved).await.unwrap();

    let next = enumerate_since(store.as_ref(), w, &first.marks)
        .await
        .unwrap();
    assert_eq!(next.read, vec![moved]);
}

#[tokio::test]
async fn the_whole_lifecycle_issues_no_list() {
    let acc = Accounted::new(MemoryStore::new());
    let store = Arc::new(acc.as_tenant(BILL));
    let w = width(4);
    seed(Arc::clone(&store), w, 40).await;
    enumerate(store.as_ref(), w).await.unwrap();
    enumerate_since(store.as_ref(), w, &Default::default())
        .await
        .unwrap();
    assert_eq!(acc.total(OpClass::List), 0);
}

#[tokio::test]
async fn a_refused_read_is_an_error_not_a_shorter_answer() {
    let w = width(4);
    // One `observe` per tenant is one read, so the seeding reads are ordinals 0 and 1, the
    // four folds are 2..=5, and enumeration's first pointer read is ordinal 6.
    let store = Arc::new(Flaky::refusing_reads_at(&[6]));
    let app = Appender::new(Arc::clone(&store), w);
    for i in 0..2u128 {
        app.observe(&TenantRecord::live(TenantId(i), Epoch(1), &[]))
            .await
            .unwrap();
    }
    for b in w.all() {
        fold(store.as_ref(), b).await.unwrap();
    }

    // ⚠️ Derived pointer keys 404 by design, so the code has to swallow absence. The same
    // branch swallowing a throttle or a 500 gives an enumeration that silently under-reports
    // and returns `Ok` -- and every count, depth and no-LIST assertion still passes.
    assert!(enumerate(store.as_ref(), w).await.is_err());
    assert_eq!(store.failures(), 1, "no read was actually refused");
}
