//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The engine's half of a scheduled reap (M18): what is due is decided from memory, and a
//! reap is bounded by time -- through the highest epoch this engine committed `age` ago.

use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass};
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_types::{Epoch, LaneId, TenantId};
use std::sync::Arc;
use std::time::Duration;

const HOUR: Duration = Duration::from_secs(3600);

fn doc(id: &str) -> Document {
    Document::new(id, vec![1.0, 0.5])
}

async fn commit<S: BlobStore + 'static>(e: &Engine<S>, id: &str) -> Epoch {
    e.write("idx", vec![doc(id)]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap()
}

async fn bundles(store: &MemoryStore) -> usize {
    store
        .list_unrestricted(&Key::new(String::new()))
        .await
        .unwrap()
        .iter()
        .filter(|k| k.as_str().ends_with(".bundle"))
        .count()
}

#[tokio::test(start_paused = true)]
async fn nothing_is_due_until_a_commit_is_age_old() {
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(180), LaneId(1));
    assert_eq!(e.reap_due(HOUR), None, "due with nothing committed");
    let first = commit(&e, "a").await;
    assert_eq!(e.reap_due(HOUR), None);
    tokio::time::advance(HOUR - Duration::from_secs(1)).await;
    assert_eq!(e.reap_due(HOUR), None, "due a second early");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(e.reap_due(HOUR), Some(first));
}

#[tokio::test(start_paused = true)]
async fn a_reap_takes_what_it_buried_and_forgets_its_records() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(181), LaneId(1));
    for id in ["a", "b", "c"] {
        commit(&e, id).await;
    }
    let last = e.compact("idx").await.unwrap().unwrap();
    assert!(bundles(&store).await >= 3);
    tokio::time::advance(HOUR).await;
    let horizon = e.reap_due(HOUR).unwrap();
    assert_eq!(horizon, last, "not the highest epoch an hour old");
    assert!(e.gc_through(horizon).await.unwrap() > 0, "nothing reaped");
    assert_eq!(bundles(&store).await, 0, "folded bundles survived");
    let head = e.head_for_test().await;
    assert_eq!(head.reaped_before, horizon.0);
    assert!(
        head.graveyard.keys().all(|b| *b > horizon.0),
        "{:?}",
        head.graveyard
    );
    // The reap's own commit is no record, and nothing has committed since.
    assert_eq!(e.reap_due(HOUR), None);
    tokio::time::advance(HOUR).await;
    assert_eq!(e.reap_due(HOUR), None, "the reap made itself due");
    let reader = Engine::new(Arc::clone(&store), TenantId(181), LaneId(9));
    assert_eq!(reader.scan("idx", None).await.unwrap().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_reap_that_finds_nothing_still_forgets_its_records() {
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(182), LaneId(1));
    commit(&e, "a").await;
    // An operator's reap leaves the scheduled one nothing to do.
    e.gc(0).await.unwrap();
    tokio::time::advance(HOUR).await;
    let horizon = e.reap_due(HOUR).unwrap();
    assert_eq!(e.gc_through(horizon).await.unwrap(), 0);
    assert_eq!(
        e.reap_due(HOUR),
        None,
        "a record survived a reap that found nothing"
    );
}

#[tokio::test(start_paused = true)]
async fn records_younger_than_age_survive_a_reap() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(183), LaneId(1));
    let first = commit(&e, "a").await;
    tokio::time::advance(HOUR / 2).await;
    let second = commit(&e, "b").await;
    tokio::time::advance(HOUR / 2).await;
    assert_eq!(
        e.reap_due(HOUR),
        Some(first),
        "the younger commit was counted"
    );
    e.gc_through(first).await.unwrap();
    // The second fold's bundle is buried after the horizon, so it stays.
    assert_eq!(bundles(&store).await, 1);
    assert_eq!(e.reap_due(HOUR), None);
    tokio::time::advance(HOUR / 2).await;
    assert_eq!(e.reap_due(HOUR), Some(second));
}

#[tokio::test]
async fn gc_by_retention_still_reads_head_once() {
    let acct = Accounted::new(MemoryStore::new());
    let t = TenantId(184);
    let e = Engine::new(Arc::new(acct.as_tenant(t)), t, LaneId(1));
    commit(&e, "a").await;
    let reads = acct.count(t, OpClass::Read);
    assert!(e.gc(0).await.unwrap() > 0);
    assert_eq!(acct.count(t, OpClass::Read) - reads, 1);
}
