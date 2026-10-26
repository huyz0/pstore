//! `Engine::gc` must not hand the store a batch larger than the store accepts.
//!
//! ⚠️ Carried from M7a, where making `ObjectStoreBackend` refuse over-cap batches — matching
//! what `MemoryStore` had always done — made this failure *uniform* instead of
//! backend-dependent. It was already live: every backend's `delete_batch` is documented
//! "capped at `Capabilities::max_batch_delete`", and `gc` built its list from the graveyard
//! with no bound at all.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::{Accounted, BlobStore, MemoryStore, OpClass};
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_types::{Epoch, LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(1);

fn doc(id: u64) -> Document {
    Document::new(id.to_string(), vec![1.0, 0.0])
}

/// A store whose delete cap is small, so the boundary is reachable in a test.
///
/// ⚠️ The cap is `Capabilities`' to state, not a constant of ours — a backend that accepts
/// 256 (Azure) and one that accepts 1000 (S3) are both correct, and `gc` has to read it
/// rather than know it.
fn capped(n: usize) -> MemoryStore {
    MemoryStore::with_max_batch_delete(n)
}

#[tokio::test]
async fn gc_never_exceeds_the_backends_delete_cap() {
    let acc = Accounted::new(capped(4));
    let store = Arc::new(acc.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(0));

    // Enough folds that the graveyard holds more dead objects than one batch can carry.
    for i in 0..12u64 {
        e.write("i", vec![doc(i)]).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    let before = e.head_for_test().await;
    assert!(
        before.graveyard.values().map(Vec::len).sum::<usize>() > 4,
        "the fixture must give gc more to reap than one batch holds"
    );

    // The bug: one unbounded `delete_batch`, refused by the store, so gc never reaps.
    let reaped = e.gc(0).await.expect("gc must not exceed the cap");
    assert!(reaped > 0, "gc reaped nothing");
    assert!(acc.total(OpClass::Delete) > 1, "gc issued a single batch");

    // And it finished the job: nothing the graveyard named is still there.
    let after = e.head_for_test().await;
    assert!(after.graveyard.is_empty(), "{:?}", after.graveyard);
    for keys in before.graveyard.values() {
        for k in keys {
            assert!(
                store.get(&pstore_blob::Key::new(k.clone())).await.is_err(),
                "{k} survived gc"
            );
        }
    }
}

#[tokio::test]
async fn a_graveyard_within_the_cap_is_still_one_batch() {
    // The cap is a ceiling, not a chunk size: gc must not turn a legal batch into many.
    let acc = Accounted::new(capped(1000));
    let store = Arc::new(acc.as_tenant(T));
    let e = Engine::new(Arc::clone(&store), T, LaneId(0));
    for i in 0..3u64 {
        e.write("i", vec![doc(i)]).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    let epoch = e.head_for_test().await.epoch;
    assert!(epoch > Epoch::ZERO);

    let before = acc.total(OpClass::Delete);
    e.gc(0).await.unwrap();
    assert_eq!(acc.total(OpClass::Delete) - before, 1);
}

#[tokio::test]
async fn a_cap_of_zero_is_an_error_and_never_a_panic() {
    // ⚠️ `slice::chunks(0)` **panics**, and a capability is a value read from a recorded
    // profile — so a malformed or defaulted profile would take the process down inside a
    // background collector. The `.max(1)` guard turns that into the store's own refusal,
    // which is an error a caller can act on. Nothing else in the suite reaches a cap of zero,
    // so without this the guard is untested and the mutation survives.
    let store = Arc::new(capped(0));
    let e = Engine::new(Arc::clone(&store), T, LaneId(0));
    e.write("i", vec![doc(1)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.write("i", vec![doc(2)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();

    assert!(
        !e.head_for_test().await.graveyard.is_empty(),
        "the fixture must give gc something to try to reap"
    );
    assert!(
        e.gc(0).await.is_err(),
        "a cap of zero must refuse, not panic"
    );
}
