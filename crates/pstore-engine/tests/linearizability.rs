//! The epoch sequence under adversarial scheduling, at the scale the milestone names.
//!
//! HEAD is a linearizable register and the epoch is the version it carries. Everything
//! above it — read-after-write, fencing, the retention window GC reaps against — assumes
//! two properties that no amount of care in the commit protocol can be trusted to give
//! for free:
//!
//! - **Dense.** Epoch *n* implies a manifest at every epoch below it. A gap means a number
//!   was consumed without a commit behind it, and any component that walks epochs
//!   backwards then walks off a cliff.
//! - **Never reused.** Two different manifests at the same epoch is a lost update wearing
//!   a valid-looking version number.
//!
//! Neither is checked by the commit path itself, deliberately: a protocol that validates
//! its own output tests nothing.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

use pstore_blob::MemoryStore;
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_testkit::flaky::Flaky;
use pstore_types::{Epoch, LaneId, TenantId};
use std::collections::BTreeSet;
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

/// The scale the exit condition names. Kept as a constant so lowering it is a visible
/// edit rather than a quiet one.
const WRITERS: u64 = 100;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_hundred_writers_produce_a_dense_epoch_sequence() {
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(1);

    let mut tasks = Vec::new();
    for w in 0..WRITERS {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            let e = Engine::new(store, t, LaneId(w));
            e.write("idx", vec![doc(&format!("w{w}"))]).await.unwrap();
            e.flush().await.unwrap();
            e.fold().await.unwrap()
        }));
    }
    let mut observed: Vec<Epoch> = Vec::new();
    for task in tasks {
        observed.push(task.await.unwrap());
    }

    // Every epoch a commit ever reported, deduplicated. A tenant-scoped fold that finds
    // its work already done reports the epoch it found, so repeats in this list are
    // expected; gaps in the deduplicated sequence are not.
    let distinct: BTreeSet<u64> = observed.iter().map(|e| e.0).collect();
    let top = *distinct.iter().next_back().unwrap();
    let missing: Vec<u64> = (1..=top).filter(|n| !distinct.contains(n)).collect();
    assert!(
        missing.is_empty(),
        "the epoch sequence has {} gaps below {top}: {missing:?}",
        missing.len()
    );

    // And the register agrees with what the writers were told: a HEAD behind the highest
    // reported epoch would mean a commit was acknowledged and then lost.
    let reader = Engine::new(Arc::clone(&store), t, LaneId(9_999));
    assert_eq!(reader.fold().await.unwrap(), Epoch(top));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn every_acknowledged_document_survives_contention() {
    // 100 writers, 3 batches each: 300 rows that must all be visible, exactly once.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(2);

    let mut tasks = Vec::new();
    for w in 0..WRITERS {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            let e = Engine::new(store, t, LaneId(w));
            for b in 0..3 {
                e.write("idx", vec![doc(&format!("w{w}-b{b}"))])
                    .await
                    .unwrap();
                e.flush().await.unwrap();
                e.fold().await.unwrap();
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }

    let reader = Engine::new(Arc::clone(&store), t, LaneId(9_999));
    reader.fold().await.unwrap();
    let ids: Vec<String> = reader
        .scan("idx", None)
        .await
        .unwrap()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    let distinct: BTreeSet<&String> = ids.iter().collect();
    let want = (WRITERS as usize) * 3;
    assert_eq!(
        distinct.len(),
        want,
        "{} of {want} rows survived",
        distinct.len()
    );
    assert_eq!(
        ids.len(),
        want,
        "{} rows for {want} writes: a duplicate",
        ids.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_writer_paused_past_many_commits_cannot_corrupt() {
    // The fencing property at scale. A writer reads HEAD, then stalls while a hundred
    // commits land. When it wakes its view is ancient — and the CAS, not any timeout or
    // lease, is what stops it from overwriting a world it never saw.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(3);

    let paused = Engine::new(Arc::clone(&store), t, LaneId(0));
    paused.write("idx", vec![doc("stale")]).await.unwrap();
    paused.flush().await.unwrap();
    paused.fold().await.unwrap();

    for w in 1..=WRITERS {
        let e = Engine::new(Arc::clone(&store), t, LaneId(w));
        e.write("idx", vec![doc(&format!("live{w}"))])
            .await
            .unwrap();
        e.flush().await.unwrap();
        e.fold().await.unwrap();
    }

    assert!(
        paused.commit_stale_for_test().await.is_err(),
        "a writer {WRITERS} commits behind was allowed to commit"
    );

    // Nothing it touched is damaged: every row still there, exactly once.
    let reader = Engine::new(Arc::clone(&store), t, LaneId(9_999));
    reader.fold().await.unwrap();
    let ids: Vec<String> = reader
        .scan("idx", None)
        .await
        .unwrap()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    let distinct: BTreeSet<&String> = ids.iter().collect();
    assert_eq!(distinct.len(), WRITERS as usize + 1);
    assert_eq!(ids.len(), WRITERS as usize + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_cas_storm_still_converges() {
    // Every writer commits at once against a store that also refuses a fifth of its
    // writes. The failure mode this bounds is livelock: a backoff that does not spread
    // writers out leaves them colliding until the retry budget runs out, which surfaces
    // as a *flaky* test rather than a failing one — so the assertion is on the retry
    // budget being enough for all of them, not on any one attempt.
    let store = Arc::new(Flaky::new(0xC0FFEE, 0.2));
    let t = TenantId(4);

    let mut tasks = Vec::new();
    for w in 0..WRITERS {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            let e = Engine::new(store, t, LaneId(w));
            // Retried because the STORE is refusing, which is a transient fault and not
            // a contention failure. Contention is the engine's own problem, and if it
            // needs help from this loop the engine is what is broken.
            for _ in 0..20 {
                if e.write("idx", vec![doc(&format!("w{w}"))]).await.is_ok()
                    && e.flush().await.is_ok()
                {
                    break;
                }
            }
            for _ in 0..20 {
                if e.fold().await.is_ok() {
                    return true;
                }
            }
            false
        }));
    }
    let mut converged = 0usize;
    for task in tasks {
        if task.await.unwrap() {
            converged += 1;
        }
    }
    assert_eq!(
        converged, WRITERS as usize,
        "{converged} of {WRITERS} writers converged: the rest exhausted their retries"
    );
    assert!(store.failures() > 20, "the storm injected no faults");
}
