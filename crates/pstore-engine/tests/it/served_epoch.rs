//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! An answer carries the epoch of the manifest it came from (M10, BACKLOG row 40): the HEAD a
//! live query read, or the epoch an `as_of` query asked for -- never this engine's last commit.

use pstore_blob::MemoryStore;
use pstore_engine::{Consistency, Engine};
use pstore_format::{DEFAULT_FIELD, Document};
use pstore_query::{Fusion, OrderBy, Prefetch};
use pstore_types::{Epoch, LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(1001);

fn dense() -> [Prefetch; 1] {
    [Prefetch::Dense {
        field: DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.5],
        limit: 10,
        tune: pstore_index::vec_index::Query::default(),
    }]
}

fn by_id() -> OrderBy {
    OrderBy {
        attr: "id".to_owned(),
        desc: false,
    }
}

/// A reader that never committed, over a HEAD another engine folded twice.
async fn reader() -> Engine<MemoryStore> {
    let s = Arc::new(MemoryStore::new());
    let w = Engine::new(Arc::clone(&s), T, LaneId(1));
    for id in ["a", "b"] {
        w.write("idx", vec![Document::new(id, vec![1.0, 0.5])])
            .await
            .unwrap();
        w.flush().await.unwrap();
        w.fold().await.unwrap();
    }
    assert_eq!(w.epoch(), Epoch(2));
    let r = Engine::new(s, T, LaneId(2));
    assert_eq!(r.epoch(), Epoch::ZERO);
    r
}

#[tokio::test]
async fn a_live_answer_carries_the_heads_epoch() {
    let r = reader().await;
    for index in ["idx", "never-written"] {
        let a = r
            .query_filtered_as(
                index,
                &dense(),
                None,
                Fusion::default(),
                10,
                Consistency::Eventual,
            )
            .await
            .unwrap();
        assert_eq!(a.epoch, Epoch(2), "{index}");
        let o = r
            .ordered_as(index, &by_id(), None, 0, 10, None, Consistency::Eventual)
            .await
            .unwrap();
        assert_eq!(o.epoch, Epoch(2), "{index}");
    }
}

#[tokio::test]
async fn a_past_answer_carries_the_epoch_it_asked_for() {
    let r = reader().await;
    for index in ["idx", "never-written"] {
        let a = r
            .query_as_of_filtered(index, Epoch(1), &dense(), None, Fusion::default(), 10)
            .await
            .unwrap();
        assert_eq!(a.epoch, Epoch(1), "{index}");
        let o = r
            .ordered_as(
                index,
                &by_id(),
                None,
                0,
                10,
                Some(Epoch(1)),
                Consistency::Eventual,
            )
            .await
            .unwrap();
        assert_eq!(o.epoch, Epoch(1), "{index}");
    }
    // And it is the past: one row then, two now.
    let a = r
        .query_as_of_filtered("idx", Epoch(1), &dense(), None, Fusion::default(), 10)
        .await
        .unwrap();
    assert_eq!(a.hits.len(), 1);
}

#[tokio::test]
async fn stats_of_a_missing_index_carry_the_heads_epoch() {
    // M10.2: the epoch of the HEAD read, even when HEAD names no such index.
    let r = reader().await;
    let (epoch, stats) = r.index_stats_at("never-written").await.unwrap();
    assert_eq!(epoch, Epoch(2));
    assert!(stats.is_none());
    let (epoch, stats) = r.index_stats_at("idx").await.unwrap();
    assert_eq!(epoch, Epoch(2));
    assert_eq!(stats.unwrap().epoch, Epoch(2));
}
