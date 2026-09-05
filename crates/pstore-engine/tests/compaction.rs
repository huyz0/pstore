//! Compaction as optimistic work.
//!
//! Nothing coordinates compactors. Any node may merge any index at any time, several may
//! merge the same inputs at once, and the only serialization point is the CAS that
//! publishes the result. That is a deliberate trade: duplicate *work* is cheap and
//! duplicate *commits* are impossible, so the design pays CPU to avoid a lock.
//!
//! What has to hold for that trade to be sound is what this file checks — exactly one
//! commit lands, the answer does not change, and a loser's object is unreferenced from
//! the moment it loses.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::{BlobStore, MemoryStore};
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_testkit::gated::Gated;
use pstore_types::{LaneId, TenantId};
use std::collections::BTreeSet;
use std::sync::Arc;

fn doc(id: &str, v: f32) -> Document {
    Document {
        id: id.to_owned(),
        vector: vec![v; 4],
        attrs: Default::default(),
    }
}

/// Builds an index with `n` separate segments, one per fold.
async fn with_segments<S: BlobStore>(store: &Arc<S>, t: TenantId, n: usize) -> Engine<S> {
    let e = Engine::new(Arc::clone(store), t, LaneId(1));
    for i in 0..n {
        e.write("idx", vec![doc(&format!("d{i}"), i as f32)])
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }
    e
}

#[tokio::test]
async fn compaction_does_not_change_the_answer() {
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(1);
    let e = with_segments(&store, t, 6).await;

    let before: Vec<String> = e
        .scan("idx", None)
        .await
        .unwrap()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    assert_eq!(before.len(), 6);

    assert!(
        e.compact("idx").await.unwrap().is_some(),
        "nothing was compacted"
    );

    // Read by a node that did not compact, so the answer comes from the published
    // manifest rather than from the compactor's own memory.
    let reader = Engine::new(Arc::clone(&store), t, LaneId(99));
    let after: Vec<String> = reader
        .scan("idx", None)
        .await
        .unwrap()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    assert_eq!(after, before, "the merge changed the answer");
}

#[tokio::test]
async fn compaction_collapses_the_segments_it_merged() {
    // Without this, "does not change the answer" is satisfied by a compaction that does
    // nothing at all.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(2);
    let e = with_segments(&store, t, 6).await;
    e.compact("idx").await.unwrap().unwrap();

    let reader = Engine::new(Arc::clone(&store), t, LaneId(99));
    let head = reader.head_for_test().await;
    assert_eq!(
        head.indexes.get("idx").map(Vec::len),
        Some(1),
        "six segments did not become one"
    );
}

#[tokio::test]
async fn a_short_index_is_not_worth_compacting() {
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(3);
    let e = with_segments(&store, t, 1).await;
    assert!(
        e.compact("idx").await.unwrap().is_none(),
        "a single segment was merged with itself"
    );
    assert!(e.compact("absent").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_compactors_produce_one_winner() {
    // ⚠️ Twelve compactors, released together. Spawning twelve tasks and trusting the
    // scheduler to overlap them is a lottery: a compactor that happens to read HEAD after
    // the winner published sees one segment and returns without racing at all, and the
    // test then passes for the wrong reason -- or fails for one, which is how this was
    // found.
    let store = Arc::new(Gated::new(12));
    let t = TenantId(4);
    let seeded = with_segments(&store, t, 8).await;
    let before: Vec<String> = seeded
        .scan("idx", None)
        .await
        .unwrap()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    drop(seeded);
    store.arm();

    // Twelve nodes decide to compact the same index at the same moment, which is what
    // happens when a policy is evaluated from the same manifest by every node that reads
    // it. No node is told about the others.
    let mut tasks = Vec::new();
    for w in 0..12u64 {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            Engine::new(store, t, LaneId(w))
                .compact("idx")
                .await
                .unwrap()
        }));
    }
    let mut winners = 0usize;
    for task in tasks {
        if task.await.unwrap().is_some() {
            winners += 1;
        }
    }
    // ⚠️ Asserted BEFORE the winner count. "Exactly one winner" is equally satisfied by
    // twelve compactors racing and by one compactor running alone, so without this the
    // headline assertion can pass having tested nothing.
    assert!(store.raced(), "the compactors never met at the barrier");
    assert_eq!(
        winners, 1,
        "{winners} compactors committed; exactly one may win and the rest must discard"
    );

    let reader = Engine::new(Arc::clone(&store), t, LaneId(99));
    let after: Vec<String> = reader
        .scan("idx", None)
        .await
        .unwrap()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    assert_eq!(after, before, "the race changed the answer");
    assert_eq!(reader.head_for_test().await.indexes["idx"].len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_losing_compactors_output_is_unreferenced() {
    // The losers wrote real objects. GC's whole contract is that an object HEAD does not
    // name is reapable, so if a loser's output were still referenced -- or if the winner's
    // were not -- the manifest and the bucket would disagree about what is live.
    let store = Arc::new(Gated::new(6));
    let t = TenantId(5);
    drop(with_segments(&store, t, 8).await);
    store.arm();

    let mut tasks = Vec::new();
    for w in 0..6u64 {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            Engine::new(store, t, LaneId(w))
                .compact("idx")
                .await
                .unwrap()
        }));
    }
    for task in tasks {
        let _ = task.await.unwrap();
    }

    let reader = Engine::new(Arc::clone(&store), t, LaneId(99));
    let referenced: BTreeSet<String> = reader
        .head_for_test()
        .await
        .indexes
        .values()
        .flatten()
        .map(|r| r.key.clone())
        .collect();

    // ⚠️ The one place a LIST is legitimate: this is GC's view, not a read path.
    let all: Vec<String> = store
        .list_unrestricted(&pstore_blob::Key::new(String::new()))
        .await
        .unwrap()
        .iter()
        .map(|k| k.as_str().to_owned())
        .filter(|k| k.contains("/seg/L1/"))
        .collect();

    assert!(
        all.len() > 1,
        "only {} L1 objects: the race did not race",
        all.len()
    );
    let orphans: Vec<&String> = all.iter().filter(|k| !referenced.contains(*k)).collect();
    assert_eq!(
        orphans.len(),
        all.len() - 1,
        "every compactor but the winner must leave its output unreferenced"
    );
    // And the converse: nothing HEAD names is missing from the bucket.
    for key in &referenced {
        assert!(
            store
                .head(&pstore_blob::Key::new(key.clone()))
                .await
                .is_ok(),
            "HEAD names {key}, which does not exist"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fold_landing_mid_compaction_is_not_swallowed_by_it() {
    // A compactor decides what an index contains, then takes time to merge it. Meanwhile
    // a fold publishes a new segment. Whichever wins the CAS, the loser rebases -- and if
    // the compactor rebuilds the index from the segments it *read* rather than from the
    // ones HEAD now names, the fold's rows vanish with no error anywhere.
    let store = Arc::new(Gated::new(2));
    let t = TenantId(6);
    drop(with_segments(&store, t, 6).await);

    let folder = Engine::new(Arc::clone(&store), t, LaneId(2));
    folder.write("idx", vec![doc("seeded", 0.0)]).await.unwrap();
    folder.flush().await.unwrap();
    folder.fold().await.unwrap();

    // Buffered and made durable before the gate, so the fold below is a pure commit and
    // arrives at the barrier together with the compaction.
    folder
        .write("idx", vec![doc("mid-flight", 1.0)])
        .await
        .unwrap();
    folder.flush().await.unwrap();
    store.arm();

    let compactor = Engine::new(Arc::clone(&store), t, LaneId(3));
    let (c, f) = tokio::join!(compactor.compact("idx"), folder.fold());
    c.unwrap();
    f.unwrap();
    assert!(
        store.raced(),
        "the fold and the compaction never overlapped"
    );

    let reader = Engine::new(Arc::clone(&store), t, LaneId(99));
    let ids: Vec<String> = reader
        .scan("idx", None)
        .await
        .unwrap()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    let distinct: BTreeSet<&String> = ids.iter().collect();
    assert!(
        distinct.contains(&"mid-flight".to_owned()),
        "the fold's rows were swallowed by the compaction: {ids:?}"
    );
    assert_eq!(
        distinct.len(),
        8,
        "{} of 8 rows survived: {ids:?}",
        distinct.len()
    );
    assert_eq!(ids.len(), 8, "{} rows for 8 writes: a duplicate", ids.len());
}
