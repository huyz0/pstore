//! Criteria 2, 7, 8, 10 and 14: what recording a tenant costs, and what bounds it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_catalog::{Appender, MAX_PENDING, TenantRecord, Width, fold, read_head};
use pstore_testkit::gated::Gated;
use pstore_types::{Epoch, TenantId};
use std::sync::Arc;

/// Everything the catalog does is billed here. The catalog is not per-tenant, so it all lands
/// on one synthetic id — the counter's shape, not a claim about attribution.
const BILL: TenantId = TenantId(0);

fn indexes(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("index-{i:04}")).collect()
}

/// ⚠️ Writes the deployment's root before a census. `enumerate` refuses a width the root does
/// not name, and an **absent** root means "never widened" — the default width. A fixture at
/// width 1 with no root models a deployment that cannot exist.
async fn rooted<S: pstore_blob::BlobStore>(store: &S, width: Width) {
    let _ = pstore_catalog::write_root(
        store,
        pstore_catalog::Root {
            epoch: pstore_types::Epoch(1),
            width,
        },
        None,
    )
    .await;
}

fn one() -> Width {
    Width::new(1).expect("one bucket")
}

#[tokio::test]
async fn an_observe_costs_one_read_and_one_write() {
    let store = Accounted::new(MemoryStore::new());
    let app = Appender::new(Arc::new(store.as_tenant(BILL)), one());

    assert!(
        app.observe(&TenantRecord::live(TenantId(1), Epoch(1), &indexes(3)))
            .await
            .unwrap()
    );

    // ⚠️ One read, not two: the head and the tag it will be conditioned on come back
    // together. Reading them separately is a lost update as well as a wasted request.
    assert_eq!(store.count(BILL, OpClass::Read), 1);
    assert_eq!(store.count(BILL, OpClass::Write), 1);
    assert_eq!(store.count(BILL, OpClass::List), 0);
}

#[tokio::test]
async fn an_unchanged_index_set_writes_nothing() {
    let store = Accounted::new(MemoryStore::new());
    let app = Appender::new(Arc::new(store.as_tenant(BILL)), one());
    let idx = indexes(3);

    assert!(
        app.observe(&TenantRecord::live(TenantId(1), Epoch(1), &idx))
            .await
            .unwrap()
    );
    let (r, w) = (
        store.count(BILL, OpClass::Read),
        store.count(BILL, OpClass::Write),
    );

    // ⚠️ The epoch advances on every commit and the index set does not. If the change check
    // looked at the epoch, this loop would be 100 appends -- a request that scales with
    // records, which is what C-12's whole affordability argument rests on not happening.
    for e in 2..=100 {
        assert!(
            !app.observe(&TenantRecord::live(TenantId(1), Epoch(e), &idx))
                .await
                .unwrap()
        );
    }
    assert_eq!(store.count(BILL, OpClass::Read), r);
    assert_eq!(store.count(BILL, OpClass::Write), w);

    // A real change still gets through.
    assert!(
        app.observe(&TenantRecord::live(TenantId(1), Epoch(101), &indexes(4)))
            .await
            .unwrap()
    );
    assert_eq!(store.count(BILL, OpClass::Write), w + 1);
}

#[tokio::test]
async fn racing_appenders_both_land() {
    // ⚠️ A barrier, not a `spawn` and a hope. Both writers reach `put_conditional` holding a
    // precondition from the same world before either lands, so the loser genuinely has to
    // rebase -- and a rebase that retried its *original* head instead of re-reading would
    // drop whatever the winner put there.
    let store = Arc::new(Gated::new(2));
    let a = Appender::new(Arc::clone(&store), one());
    let b = Appender::new(Arc::clone(&store), one());

    let (r1, r2) = (
        TenantRecord::live(TenantId(1), Epoch(1), &indexes(1)),
        TenantRecord::live(TenantId(2), Epoch(1), &indexes(1)),
    );
    store.arm();
    let (ra, rb) = tokio::join!(a.observe(&r1), b.observe(&r2));
    assert!(ra.unwrap() && rb.unwrap());
    // ⚠️ Without this the test is a lottery that a single-threaded runtime wins by running
    // the two appenders in sequence -- which is the case where nothing has to rebase at all.
    assert!(store.raced(), "the racers never met at the barrier");

    let (head, _) = read_head(store.as_ref(), 0).await.unwrap();
    let mut ids: Vec<_> = head.pending.iter().map(|r| r.tenant.0).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

#[tokio::test]
async fn pending_is_bounded_by_an_inline_fold() {
    let store = Arc::new(MemoryStore::new());
    let app = Appender::new(Arc::clone(&store), one());

    for i in 0..(MAX_PENDING as u128 * 3 + 1) {
        app.observe(&TenantRecord::live(TenantId(i), Epoch(1), &indexes(2)))
            .await
            .unwrap();
        let (head, _) = read_head(store.as_ref(), 0).await.unwrap();
        assert!(
            head.pending.len() <= MAX_PENDING,
            "after {i} records pending is {}",
            head.pending.len()
        );
    }

    // The inline folds are what kept it bounded, and they must not have lost anything.
    rooted(store.as_ref(), one()).await;
    let all = pstore_catalog::enumerate(store.as_ref(), one())
        .await
        .unwrap();
    assert_eq!(all.records.len(), MAX_PENDING * 3 + 1);
}

#[tokio::test]
async fn an_observe_moves_fewer_bytes_than_a_fold() {
    // C-12's cheap side, measured. A bucket has to actually hold more than the cap for the
    // comparison to mean anything -- at four records in the run, pending IS the run.
    let store = Accounted::new(MemoryStore::new());
    let app = Appender::new(Arc::new(store.as_tenant(BILL)), one());
    for i in 0..64u128 {
        app.observe(&TenantRecord::live(TenantId(i), Epoch(1), &indexes(10)))
            .await
            .unwrap();
    }
    fold(&store.as_tenant(BILL), 0).await.unwrap();
    // ⚠️ Measured at the cap, not at an empty pointer. An observe onto an *empty* pending
    // list is cheap whatever `MAX_PENDING` is, so a fixture that stopped here would report
    // the trade as free however far the cap were raised -- which is the number this test is
    // about.
    for i in 100..(100 + MAX_PENDING as u128 - 1) {
        app.observe(&TenantRecord::live(TenantId(i), Epoch(1), &indexes(10)))
            .await
            .unwrap();
    }

    let before = store.bytes(BILL, OpClass::Read) + store.bytes(BILL, OpClass::Write);
    app.observe(&TenantRecord::live(TenantId(1000), Epoch(1), &indexes(10)))
        .await
        .unwrap();
    let observe = store.bytes(BILL, OpClass::Read) + store.bytes(BILL, OpClass::Write) - before;

    let before = store.bytes(BILL, OpClass::Read) + store.bytes(BILL, OpClass::Write);
    assert!(fold(&store.as_tenant(BILL), 0).await.unwrap());
    let folded = store.bytes(BILL, OpClass::Read) + store.bytes(BILL, OpClass::Write) - before;

    assert!(
        observe * 4 < folded,
        "observe moved {observe} bytes against a fold's {folded}: the pointer now costs what \
         the run costs, and carrying pending in it has bought nothing"
    );
}
