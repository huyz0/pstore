//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A writer restarted on its own lane resumes it (M9j, BACKLOG row 39). A lane is dense and
//! single-writer, so a successor on the SAME lane must continue at the tail, not at zero:
//! zero overwrites bundles already written, folded or not, and those rows are gone.

use pstore_blob::{Accounted, MemoryStore, OpClass};
use pstore_engine::{Consistency, Engine, EngineError};
use pstore_format::{DEFAULT_FIELD, Document};
use pstore_query::{Fusion, Prefetch};
use pstore_testkit::flaky::Flaky;
use pstore_types::{LaneId, Seq, TenantId};
use std::sync::Arc;

const LANE: LaneId = LaneId(7);

fn doc(id: &str) -> Document {
    Document::new(id, vec![1.0, 0.5])
}

async fn ids<S: pstore_blob::BlobStore>(e: &Engine<S>) -> Vec<String> {
    let mut ids: Vec<String> = e
        .scan("idx", None)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    ids.sort();
    ids
}

async fn put<S: pstore_blob::BlobStore>(e: &Engine<S>, id: &str) -> Option<Seq> {
    e.write("idx", vec![doc(id)]).await.unwrap();
    e.flush().await.unwrap()
}

async fn strong<S: pstore_blob::BlobStore>(e: &Engine<S>) -> Result<(), EngineError> {
    let dense = [Prefetch::Dense {
        field: DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.5],
        limit: 10,
        tune: pstore_index::vec_index::Query::default(),
    }];
    e.query_filtered_as(
        "idx",
        &dense,
        None,
        Fusion::default(),
        10,
        Consistency::Strong,
    )
    .await
    .map(|_| ())
}

#[tokio::test]
async fn a_restart_after_a_fold_resumes_at_the_tail() {
    let s = Arc::new(MemoryStore::new());
    let t = TenantId(901);
    {
        let e1 = Engine::new(Arc::clone(&s), t, LANE);
        put(&e1, "a").await;
        e1.fold().await.unwrap();
        put(&e1, "b").await;
        // Gone: `b` is durable and unfolded.
    }
    let e2 = Engine::new(Arc::clone(&s), t, LANE);
    assert_eq!(put(&e2, "c").await, Some(Seq(2)), "not resumed at the tail");
    // Its own write is visible before any fold: not pruned as below the watermark.
    assert!(ids(&e2).await.contains(&"c".to_owned()));
    // `b` is neither folded nor in this memtable, so `strong` must not claim to reflect it.
    assert!(matches!(strong(&e2).await, Err(EngineError::NotFolded)));

    e2.fold().await.unwrap();
    strong(&e2).await.unwrap();
    assert_eq!(ids(&e2).await, ["a", "b", "c"]);
    let reader = Engine::new(Arc::clone(&s), t, LaneId(99));
    assert_eq!(ids(&reader).await, ["a", "b", "c"]);
}

#[tokio::test]
async fn a_restart_with_nothing_folded_keeps_every_bundle() {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let t = TenantId(902);
    let s = Arc::new(acct.as_tenant(t));
    {
        let e1 = Engine::new(Arc::clone(&s), t, LANE);
        put(&e1, "a").await;
        put(&e1, "b").await;
    }
    let e2 = Engine::new(Arc::clone(&s), t, LANE);
    e2.write("idx", vec![doc("c")]).await.unwrap();
    let (reads, writes) = (acct.count(t, OpClass::Read), acct.count(t, OpClass::Write));
    assert_eq!(e2.flush().await.unwrap(), Some(Seq(2)));
    // HEAD, the registry, and one window of 8 probes; one PUT.
    assert_eq!(acct.count(t, OpClass::Read) - reads, 10);
    assert_eq!(acct.count(t, OpClass::Write) - writes, 1);
    assert_eq!(acct.count(t, OpClass::List), 0);

    // Resumed once: the next flush is the one PUT and nothing else.
    e2.write("idx", vec![doc("d")]).await.unwrap();
    let (reads, writes) = (acct.count(t, OpClass::Read), acct.count(t, OpClass::Write));
    assert_eq!(e2.flush().await.unwrap(), Some(Seq(3)));
    assert_eq!(acct.count(t, OpClass::Read) - reads, 0);
    assert_eq!(acct.count(t, OpClass::Write) - writes, 1);

    e2.fold().await.unwrap();
    assert_eq!(ids(&e2).await, ["a", "b", "c", "d"]);
}

#[tokio::test]
async fn a_failed_resume_consumes_nothing_and_is_retried() {
    // Read ordinals, counted by the store: E1's first flush resumes its new lane (HEAD, then
    // one window of 8 probes beside the registry's read: reads 0..=9) and its second reads
    // nothing. E2 reads HEAD (10), then its probes and the registry together (11..=19), so 12
    // is one of its probes, or at worst its registry read: either way inside the resume.
    let refused = [12];
    let s = Arc::new(Flaky::refusing_reads_at(&refused));
    let t = TenantId(903);
    {
        let e1 = Engine::new(Arc::clone(&s), t, LANE);
        put(&e1, "a").await;
        put(&e1, "b").await;
    }
    let e2 = Engine::new(Arc::clone(&s), t, LANE);
    e2.write("idx", vec![doc("c")]).await.unwrap();
    assert!(e2.flush().await.is_err(), "the refused resume flushed");
    assert_eq!(s.failures(), 1, "the refusal missed the probe");
    assert_eq!(e2.pending_for_test().await, ["c"]);

    assert_eq!(e2.flush().await.unwrap(), Some(Seq(2)));
    e2.fold().await.unwrap();
    assert_eq!(ids(&e2).await, ["a", "b", "c"]);
}
