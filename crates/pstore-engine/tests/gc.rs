//! Reaping, and the window that makes it safe.
//!
//! GC is the one place where being wrong destroys data rather than costing money, so it
//! is built on the narrowest possible premise: an object is reapable when the manifest
//! *recorded* it as dereferenced, and only after the tenant has committed `retention`
//! further epochs. Not when a bucket listing suggests nothing points at it, and not when
//! a timer expires.
//!
//! The two failure modes are opposites and both are fatal. Reaping too early breaks a
//! reader mid-scan with a 404 it cannot interpret. Never reaping at all is a bill that
//! grows without bound. The tests below pin each.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

fn doc(id: &str) -> Document {
    Document {
        id: id.to_owned(),
        vectors: std::collections::BTreeMap::from([(
            pstore_format::DEFAULT_FIELD.to_owned(),
            pstore_format::VectorField::dense(vec![0.0; 4]),
        )]),
        attrs: Default::default(),
    }
}

async fn seed(store: &Arc<MemoryStore>, t: TenantId, n: usize) -> Engine<MemoryStore> {
    let e = Engine::new(Arc::clone(store), t, LaneId(1));
    for i in 0..n {
        e.write("idx", vec![doc(&format!("d{i}"))]).await.unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e
}

#[tokio::test]
async fn gc_never_reaps_a_referenced_object() {
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(1);
    let e = seed(&store, t, 5).await;

    // Retention of zero: the most aggressive setting there is, and still nothing HEAD
    // names may go.
    e.gc(0).await.unwrap();

    let head = e.head_for_test().await;
    for r in head.indexes.values().flatten() {
        assert!(
            store.head(&Key::new(r.key.clone())).await.is_ok(),
            "HEAD names {}, which GC reaped",
            r.key
        );
    }
    // And the index still answers with everything.
    let reader = Engine::new(Arc::clone(&store), t, LaneId(99));
    assert_eq!(reader.scan("idx", None).await.unwrap().len(), 5);
}

#[tokio::test]
async fn gc_reaps_a_folded_bundle_once_it_is_beyond_the_window() {
    // Bundles are the bulk of the garbage: one per flush, dead the moment their rows are
    // in a segment. If they are never reaped, the WAL grows forever.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(2);
    let e = seed(&store, t, 5).await;

    let bundles = || async {
        store
            .list_unrestricted(&Key::new(String::new()))
            .await
            .unwrap()
            .iter()
            .filter(|k| k.as_str().contains("/wal/"))
            .count()
    };
    assert!(bundles().await >= 5, "the scenario wrote no bundles");

    let reaped = e.gc(0).await.unwrap();
    assert!(reaped > 0, "GC reaped nothing at all");
    assert_eq!(bundles().await, 0, "folded bundles survived GC");

    // The rows are still there: they were moved into segments, not deleted.
    let reader = Engine::new(Arc::clone(&store), t, LaneId(99));
    assert_eq!(reader.scan("idx", None).await.unwrap().len(), 5);
}

#[tokio::test]
async fn a_dereferenced_object_survives_the_retention_window() {
    // The window's whole purpose: a reader that read HEAD before a compaction is still
    // scanning the segments that compaction replaced.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(3);
    let e = seed(&store, t, 4).await;

    // What a reader holding the pre-compaction epoch would be scanning.
    let held: Vec<Key> = e
        .head_for_test()
        .await
        .indexes
        .values()
        .flatten()
        .map(|r| Key::new(r.key.clone()))
        .collect();
    assert_eq!(held.len(), 4);

    e.compact("idx").await.unwrap().unwrap();

    // A generous window. Every one of those segments is now dereferenced, and every one
    // must still be readable.
    let reaped = e.gc(100).await.unwrap();
    assert_eq!(reaped, 0, "GC reaped {reaped} objects inside the window");
    for k in &held {
        assert!(
            store.head(k).await.is_ok(),
            "a reader holding the previous epoch would 404 on {}",
            k.as_str()
        );
    }

    // Now let the tenant move on past the window, and the same objects become reapable.
    for i in 0..3 {
        e.write("idx", vec![doc(&format!("later{i}"))])
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    // ⚠️ Counted against the graveyard's OWN keys, not against the delete batch. GC also
    // derives each reaped segment's sidecars, and briefly counted those too -- which padded
    // this number threefold and let a `>=` assertion pass on objects that never existed.
    let horizon = e.head_for_test().await.epoch.0.saturating_sub(1);
    let due: usize = e
        .head_for_test()
        .await
        .graveyard
        .range(..=horizon)
        .map(|(_, keys)| keys.len())
        .sum();
    let reaped = e.gc(1).await.unwrap();
    assert_eq!(
        reaped, due,
        "GC reported {reaped} objects reaped against {due} keys in the graveyard past the \
         window"
    );
    assert!(reaped >= held.len());
    for k in &held {
        assert!(
            store.head(k).await.is_err(),
            "{} is past the window and still costing money",
            k.as_str()
        );
    }
    // Still the right answer, throughout.
    let reader = Engine::new(Arc::clone(&store), t, LaneId(99));
    assert_eq!(reader.scan("idx", None).await.unwrap().len(), 7);
}

#[tokio::test]
async fn gc_prunes_what_it_reaps_so_the_manifest_stays_bounded() {
    // HEAD is read on every query. A graveyard that only grows makes every read slower
    // and every commit larger, which is a slow leak rather than a loud one.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(4);
    let e = seed(&store, t, 12).await;
    assert!(
        !e.head_for_test().await.graveyard.is_empty(),
        "nothing was recorded as dereferenced"
    );
    e.gc(0).await.unwrap();
    assert!(
        e.head_for_test().await.graveyard.is_empty(),
        "GC reaped the objects but kept their names forever"
    );
}

#[tokio::test]
async fn gc_with_nothing_due_is_free() {
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(5);
    let e = seed(&store, t, 2).await;
    e.gc(0).await.unwrap();
    // A second pass has nothing to do, and must not burn an epoch to discover that:
    // a GC on a timer would otherwise advance the epoch forever on an idle tenant.
    let before = e.head_for_test().await.epoch;
    assert_eq!(e.gc(0).await.unwrap(), 0);
    assert_eq!(
        e.head_for_test().await.epoch,
        before,
        "an idle GC committed"
    );
}

#[tokio::test]
async fn gc_refuses_to_reap_a_key_head_still_names() {
    // Defence in depth, and deliberately unreachable through the normal API: the commit
    // protocol never buries a key it still references. That is exactly why this test
    // constructs the state by hand. If a future change to fold or compact ever produces
    // it, the difference between "GC declined" and "GC deleted the index" is this branch.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(6);
    let e = seed(&store, t, 3).await;

    let live: Vec<String> = e
        .head_for_test()
        .await
        .indexes
        .values()
        .flatten()
        .map(|r| r.key.clone())
        .collect();
    assert_eq!(live.len(), 3);

    // Bury a segment that is still referenced, at an epoch long past the window.
    let buried = live[0].clone();
    e.commit_head_for_test(|h| {
        h.graveyard.entry(1).or_default().push(buried);
    })
    .await
    .unwrap();

    e.gc(0).await.unwrap();
    for k in &live {
        assert!(
            store.head(&Key::new(k.clone())).await.is_ok(),
            "GC reaped {k}, which HEAD still names"
        );
    }
    assert_eq!(
        Engine::new(Arc::clone(&store), t, LaneId(99))
            .scan("idx", None)
            .await
            .unwrap()
            .len(),
        3
    );
}
