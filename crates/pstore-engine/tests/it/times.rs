//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M148: HEAD records when each index came into being and when a commit last wrote it.

use pstore_blob::MemoryStore;
use pstore_engine::{Applied, Engine, Metric, OperationId, keyed};
use pstore_format::Document;
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;

const T: TenantId = TenantId(148);

fn doc(id: &str) -> Document {
    Document::new(id, vec![1.0, 0.5])
}

fn at(e: &Engine<MemoryStore>, ms: u64) -> &Engine<MemoryStore> {
    e.set_clock_for_test(Some(ms));
    e
}

async fn fold(e: &Engine<MemoryStore>, index: &str, ids: &[&str]) {
    e.write(index, ids.iter().map(|id| doc(id)).collect())
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
}

async fn times(e: &Engine<MemoryStore>, index: &str) -> Option<(u64, u64)> {
    e.head_for_test().await.times.get(index).copied()
}

#[tokio::test]
async fn fold_sets_written_time_only_for_its_indexes() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    at(&e, 1_000);
    e.write("a", vec![doc("1")]).await.unwrap();
    fold(&e, "b", &["1"]).await;
    // First folds: each index comes into being, both times now.
    assert_eq!(times(&e, "a").await, Some((1_000, 1_000)));
    assert_eq!(times(&e, "b").await, Some((1_000, 1_000)));
    fold(at(&e, 2_000), "a", &["2"]).await;
    assert_eq!(times(&e, "a").await, Some((1_000, 2_000)));
    assert_eq!(
        times(&e, "b").await,
        Some((1_000, 1_000)),
        "b was not written"
    );
    // A delete is a write too.
    e.delete("b", vec!["1".into()]).await.unwrap();
    e.flush().await.unwrap();
    at(&e, 3_000).fold().await.unwrap();
    assert_eq!(times(&e, "b").await, Some((1_000, 3_000)));
    let s = e.index_stats("a").await.unwrap().unwrap();
    assert_eq!((s.created_ms, s.written_ms), (Some(1_000), Some(2_000)));
}

#[tokio::test]
async fn first_fold_creates_the_time() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    fold(at(&e, 1_000), "a", &["1"]).await;
    assert_eq!(times(&e, "b").await, None, "an index that does not exist");
    fold(at(&e, 2_000), "b", &["1"]).await;
    assert_eq!(times(&e, "b").await, Some((2_000, 2_000)));
    fold(at(&e, 3_000), "b", &["2"]).await;
    assert_eq!(times(&e, "b").await, Some((2_000, 3_000)), "created once");
}

#[tokio::test]
async fn compaction_sets_no_time() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    fold(at(&e, 1_000), "a", &["1"]).await;
    fold(at(&e, 2_000), "a", &["2"]).await;
    at(&e, 3_000).compact("a").await.unwrap().unwrap();
    assert_eq!(
        e.head_for_test().await.indexes["a"].len(),
        1,
        "it compacted"
    );
    assert_eq!(times(&e, "a").await, Some((1_000, 2_000)));
}

#[tokio::test]
async fn an_index_from_before_m148_has_no_created_time() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    fold(at(&e, 1_000), "a", &["1"]).await;
    // As an M147 HEAD: the index exists, and nothing records its times.
    e.commit_head_for_test(|h| h.times.clear()).await.unwrap();
    assert_eq!(times(&e, "a").await, None);
    let s = e.index_stats("a").await.unwrap().unwrap();
    assert_eq!((s.created_ms, s.written_ms), (None, None));
    fold(at(&e, 5_000), "a", &["2"]).await;
    assert_eq!(times(&e, "a").await, Some((0, 5_000)));
    let s = e.index_stats("a").await.unwrap().unwrap();
    assert_eq!((s.created_ms, s.written_ms), (None, Some(5_000)));
}

#[tokio::test]
async fn branch_and_drop_times() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    fold(at(&e, 1_000), "a", &["1"]).await;
    at(&e, 2_000).branch("a", "b").await.unwrap();
    assert_eq!(times(&e, "b").await, Some((2_000, 2_000)));
    assert_eq!(
        times(&e, "a").await,
        Some((1_000, 1_000)),
        "the source is not written"
    );
    at(&e, 3_000).delete_index("b").await.unwrap().unwrap();
    assert_eq!(times(&e, "b").await, None, "a drop keeps no time");
    at(&e, 4_000)
        .create_index("b", 2, Metric::DotProduct)
        .await
        .unwrap();
    assert_eq!(
        times(&e, "b").await,
        Some((4_000, 4_000)),
        "a new incarnation"
    );
}

#[tokio::test]
async fn times_never_outlive_their_index() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    fold(at(&e, 1_000), "a", &["1"]).await;
    // A delete of a name that does not exist folds, and leaves no index and no time.
    e.delete("ghost", vec!["x".into()]).await.unwrap();
    e.flush().await.unwrap();
    at(&e, 1_500).fold().await.unwrap();
    let head = e.head_for_test().await;
    assert!(!head.indexes.contains_key("ghost"));
    assert!(!head.times.contains_key("ghost"), "{:?}", head.times);
    // The same process then writes it: created by this fold, not by the delete.
    fold(at(&e, 2_000), "ghost", &["1"]).await;
    assert_eq!(times(&e, "ghost").await, Some((2_000, 2_000)));
    // An index that lives only by its quarantine, and then not at all.
    let key = format!(
        "{:04x}/tnt/{}/idx/q/quarantine/{:020}-{:016x}.q",
        T.0 as u16, T.0, 1, 1
    );
    e.commit_head_for_test(|h| {
        h.quarantine.insert("q".to_owned(), vec![(key, 1)]);
        h.times.insert("q".to_owned(), (1_000, 1_000));
    })
    .await
    .unwrap();
    assert_eq!(times(&e, "q").await, Some((1_000, 1_000)));
    let through = e.head_for_test().await.epoch;
    assert_eq!(e.discard_quarantine("q", through).await.unwrap(), Some(1));
    assert_eq!(times(&e, "q").await, None, "the time outlived its index");
    at(&e, 6_000)
        .create_index("q", 2, Metric::DotProduct)
        .await
        .unwrap();
    assert_eq!(times(&e, "q").await, Some((6_000, 6_000)));
}

#[tokio::test]
async fn a_slow_clock_never_moves_a_time_back() {
    let store = Arc::new(MemoryStore::new());
    let (fast, slow) = (
        Engine::new(Arc::clone(&store), T, LaneId(1)),
        Engine::new(Arc::clone(&store), T, LaneId(2)),
    );
    fold(at(&fast, 9_000), "a", &["1"]).await;
    fold(at(&slow, 4_000), "a", &["2"]).await;
    assert_eq!(times(&slow, "a").await, Some((9_000, 9_000)));
}

#[tokio::test]
async fn a_duplicate_key_moves_no_time() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    e.set_idempotency(true);
    fold(at(&e, 1_000), "idx", &["0"]).await;
    let key = e.idempotency_key(7).await.unwrap();
    for (op, ms) in [(10, 2_000), (11, 3_000)] {
        at(&e, ms);
        let op = OperationId(op);
        let mark = e.mark().await.unwrap();
        keyed(op, &key, false, e.write("idx", vec![doc("x")]))
            .await
            .unwrap();
        e.flush().await.unwrap();
        let got = e.await_keyed(mark, op, &key).await.unwrap();
        assert!(
            matches!(
                (op.0, &got),
                (10, Applied::Known(..)) | (11, Applied::Replayed(..))
            ),
            "{got:?}"
        );
    }
    assert_eq!(
        times(&e, "idx").await,
        Some((1_000, 2_000)),
        "the retry wrote"
    );
}

#[tokio::test]
async fn bytes_are_unknown_when_any_segment_length_is() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    fold(&e, "a", &["1"]).await;
    fold(&e, "a", &["2"]).await;
    let refs = e.head_for_test().await.indexes["a"].clone();
    assert_eq!(refs.len(), 2);
    let sum: u64 = refs.iter().map(|r| r.len.unwrap()).sum();
    let s = e.index_stats("a").await.unwrap().unwrap();
    assert_eq!(s.bytes, Some(sum));
    // As a HEAD from before M45 wrote one of them.
    e.commit_head_for_test(|h| h.indexes.get_mut("a").unwrap()[1].len = None)
        .await
        .unwrap();
    let s = e.index_stats("a").await.unwrap().unwrap();
    assert_eq!(s.bytes, None);
}

#[tokio::test]
async fn a_fold_of_only_rejected_rows_writes_its_index() {
    let store = Arc::new(MemoryStore::new());
    let (a, b) = (
        Engine::new(Arc::clone(&store), T, LaneId(1)),
        Engine::new(Arc::clone(&store), T, LaneId(2)),
    );
    fold(at(&a, 1_000), "s", &["1"]).await;
    // `b` buffers three components for `a` while it has no schema; then `a` gets one of
    // two components, as a create in between would. The next fold sets `b`'s row aside
    // and writes nothing else.
    b.write("a", vec![Document::new("wide", vec![1.0, 0.5, 0.25])])
        .await
        .unwrap();
    b.flush().await.unwrap();
    a.commit_head_for_test(|h| {
        let schema = h.schemas["s"].clone();
        h.schemas.insert("a".to_owned(), schema);
        h.indexes.insert("a".to_owned(), Vec::new());
        h.times.insert("a".to_owned(), (1_000, 1_000));
    })
    .await
    .unwrap();
    at(&a, 2_000).fold().await.unwrap();
    let head = a.head_for_test().await;
    assert_eq!(head.schema_rejects.get("a"), Some(&1), "it was rejected");
    assert!(head.indexes["a"].is_empty(), "no segment");
    assert_eq!(times(&a, "a").await, Some((1_000, 2_000)));
    assert_eq!(times(&a, "s").await, Some((1_000, 1_000)));
}

#[tokio::test]
async fn an_index_held_only_by_rejects_or_quarantine_already_existed() {
    let e = Engine::new(Arc::new(MemoryStore::new()), T, LaneId(1));
    let key = format!(
        "{:04x}/tnt/{}/idx/q/quarantine/{:020}-{:016x}.q",
        T.0 as u16, T.0, 1, 1
    );
    // As an M147 HEAD: two indexes that exist by their set-aside rows alone, with no times.
    e.commit_head_for_test(|h| {
        h.schema_rejects.insert("r".to_owned(), 1);
        h.quarantine.insert("q".to_owned(), vec![(key, 1)]);
    })
    .await
    .unwrap();
    for n in ["r", "q"] {
        fold(at(&e, 7_000), n, &["1"]).await;
        assert_eq!(
            times(&e, n).await,
            Some((0, 7_000)),
            "{n} existed before M148"
        );
    }
}
