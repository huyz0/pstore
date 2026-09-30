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

/// Every key under `index`'s own path.
async fn keys_of(store: &MemoryStore, index: &str) -> Vec<String> {
    store
        .list_unrestricted(&Key::new(String::new()))
        .await
        .unwrap()
        .iter()
        .map(|k| k.as_str().to_owned())
        .filter(|k| k.contains(&format!("/idx/{index}/")))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn every_kind_of_commit_is_reaped_on_schedule() {
    // Each engine makes one kind of commit and nothing else, so only that commit can make the
    // tenant due -- and the reap must take what it buried.
    let store = Arc::new(MemoryStore::new());
    let t = TenantId(185);
    let seed = Engine::new(Arc::clone(&store), t, LaneId(1));
    for id in ["a", "b"] {
        commit(&seed, id).await;
    }
    seed.delete("idx", vec!["a".into()]).await.unwrap();
    seed.flush().await.unwrap();
    seed.fold().await.unwrap();
    seed.write("gone", vec![doc("g")]).await.unwrap();
    seed.flush().await.unwrap();
    seed.fold().await.unwrap();
    let before: Vec<String> = keys_of(&store, "idx").await;

    let compacting = Engine::new(Arc::clone(&store), t, LaneId(2));
    let compacted = compacting.compact("idx").await.unwrap().unwrap();
    let dropping = Engine::new(Arc::clone(&store), t, LaneId(3));
    let dropped = dropping.delete_index("gone").await.unwrap().unwrap();
    let branching = Engine::new(Arc::clone(&store), t, LaneId(4));
    let branched = branching.branch("idx", "copy").await.unwrap();
    tokio::time::advance(HOUR).await;
    for (e, epoch) in [
        (&compacting, compacted),
        (&dropping, dropped),
        (&branching, branched),
    ] {
        assert_eq!(e.reap_due(HOUR), Some(epoch), "a commit was not recorded");
    }
    // The drop's reap takes the dropped index; the compaction's takes the merged inputs,
    // their delete vector and their sidecars. Reaped in epoch order, as ticks would.
    for (e, epoch) in [(&compacting, compacted), (&dropping, dropped)] {
        e.gc_through(epoch).await.unwrap();
    }
    assert!(
        keys_of(&store, "gone").await.is_empty(),
        "the dropped index survived"
    );
    let after = keys_of(&store, "idx").await;
    let survivors: Vec<&String> = before.iter().filter(|k| after.contains(k)).collect();
    assert!(
        survivors.is_empty(),
        "the compacted inputs survived: {survivors:?}"
    );
    // A branch buries nothing; its reap finds nothing, and forgets the record.
    assert_eq!(branching.gc_through(branched).await.unwrap(), 0);
    assert_eq!(branching.reap_due(HOUR), None);
}

#[tokio::test(start_paused = true)]
async fn a_scheduled_reap_costs_what_gc_does() {
    let acct = Accounted::new(MemoryStore::new());
    let t = TenantId(186);
    let e = Engine::new(Arc::new(acct.as_tenant(t)), t, LaneId(1));
    commit(&e, "a").await;
    tokio::time::advance(HOUR).await;
    let horizon = e.reap_due(HOUR).unwrap();
    let count = |c| acct.count(t, c);
    let (r, w, d) = (
        count(OpClass::Read),
        count(OpClass::Write),
        count(OpClass::Delete),
    );
    assert!(e.gc_through(horizon).await.unwrap() > 0);
    // One read of HEAD, one delete batch, and one CAS.
    assert_eq!(
        (
            count(OpClass::Read) - r,
            count(OpClass::Delete) - d,
            count(OpClass::Write) - w
        ),
        (1, 1, 1)
    );
}

#[tokio::test(start_paused = true)]
async fn records_stay_bounded_when_nothing_reaps() {
    // With no reap scheduled, commits must not keep a record each forever -- and what is kept
    // may only make a commit look younger, never older. Past the cap by a margin.
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(187), LaneId(1));
    let mut last = Epoch(0);
    let mut first = Epoch(0);
    for i in 0..1100 {
        last = commit(&e, &format!("d{i}")).await;
        tokio::time::advance(Duration::from_secs(2)).await;
        match i {
            0 => first = last,
            // Below the cap every commit two seconds apart keeps its own record, up to the cap
            // itself.
            9 => assert_eq!(e.reapable_len_for_test(), 10),
            1023 => assert_eq!(e.reapable_len_for_test(), 1024),
            1024 => {
                // One past the cap thins to the odd positions of 1025: the front, every other
                // record, and the last.
                assert_eq!(e.reapable_len_for_test(), 513);
                // The front survives: exactly the first commit is `age` old.
                assert_eq!(e.reap_due(Duration::from_secs(2 * 1025)), Some(first));
            }
            _ => {}
        }
    }
    assert_eq!(e.reapable_len_for_test(), 513 + 75);
    assert!(e.reap_due(Duration::from_secs(2)) <= Some(last));
    tokio::time::advance(HOUR).await;
    assert_eq!(e.reap_due(HOUR), Some(last));
}

#[tokio::test(start_paused = true)]
async fn a_record_takes_the_commits_of_the_second_after_its_first() {
    // A bucket is anchored at its first commit: a commit under a second after that joins it,
    // and one exactly a second after starts the next.
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(189), LaneId(1));
    commit(&e, "a").await;
    tokio::time::advance(Duration::from_millis(500)).await;
    let joined = commit(&e, "b").await;
    assert_eq!(e.reapable_len_for_test(), 1);
    // The bucket took the later epoch.
    assert_eq!(e.reap_due(Duration::ZERO), Some(joined));
    tokio::time::advance(Duration::from_millis(500)).await;
    commit(&e, "c").await;
    assert_eq!(e.reapable_len_for_test(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_tenant_committing_faster_than_a_second_is_still_reaped() {
    // Merging commits a second apart must not slide the record forward for ever.
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(188), LaneId(1));
    let age = Duration::from_secs(60);
    for i in 0..140 {
        commit(&e, &format!("d{i}")).await;
        tokio::time::advance(Duration::from_millis(500)).await;
    }
    assert!(e.reap_due(age).is_some(), "a busy tenant is never due");
}

#[tokio::test(start_paused = true)]
async fn a_tenant_past_the_record_cap_is_still_reaped() {
    // At a commit every two seconds the cap is reached before an hour passes; what it drops
    // must not be the oldest, or nothing ever grows an hour old.
    let e = Engine::new(Arc::new(MemoryStore::new()), TenantId(189), LaneId(1));
    let mut due = 0;
    for i in 0..2000 {
        commit(&e, &format!("d{i}")).await;
        tokio::time::advance(Duration::from_secs(2)).await;
        if let Some(h) = e.reap_due(HOUR) {
            e.gc_through(h).await.unwrap();
            due += 1;
        }
    }
    assert!(due > 0, "a busy tenant past the cap is never due");
    assert!(e.reapable_len_for_test() <= 1024);
}
