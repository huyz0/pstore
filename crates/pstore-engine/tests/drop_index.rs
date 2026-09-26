//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Deleting an index — M9f.2: a fold that drops it, under the lane's flush lock.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::{Engine, Metric};
use pstore_format::Document;
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Prefetch};
use pstore_types::{CasTag, Epoch, LaneId, TenantId};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, Semaphore};

fn doc(id: &str, v: Vec<f32>) -> Document {
    Document::new(id, v)
}

fn dense(q: Vec<f32>) -> Vec<Prefetch> {
    vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: q,
        limit: 100,
        tune: Query::default(),
    }]
}

/// Every id a broad query of `index` returns, sorted.
async fn ids<S: BlobStore>(e: &Engine<S>, index: &str, q: Vec<f32>) -> Vec<String> {
    let answer = e
        .query(index, &dense(q), Fusion::Rrf { k: 60.0 }, 100)
        .await
        .unwrap();
    let mut out: Vec<String> = e.resolve(&answer).into_iter().map(|(id, _)| id).collect();
    out.sort();
    out
}

#[tokio::test]
async fn a_dropped_index_is_gone_wherever_its_rows_were() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(60), LaneId(1));
    let other = Engine::new(Arc::clone(&store), TenantId(60), LaneId(2));
    // Folded rows, and a bystander index.
    e.write("x", vec![doc("folded", vec![1.0, 0.0])])
        .await
        .unwrap();
    e.write("y", vec![doc("keep", vec![1.0, 0.0])])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    // Flushed but unfolded, in another process's bundle; and pending here.
    other
        .write("x", vec![doc("elsewhere", vec![1.0, 0.0])])
        .await
        .unwrap();
    other.flush().await.unwrap();
    e.write("x", vec![doc("pending", vec![1.0, 0.0])])
        .await
        .unwrap();
    // A fresh view of the pending row is cached: the drop must not leave it served.
    assert_eq!(ids(&e, "x", vec![1.0, 0.0]).await, ["folded", "pending"]);

    let dropped = e.delete_index("x").await.unwrap();
    assert!(dropped.is_some(), "an existing index was not dropped");
    assert!(ids(&e, "x", vec![1.0, 0.0]).await.is_empty());
    assert!(!e.indexes().await.unwrap().contains(&"x".to_owned()));
    assert!(!e.pending_indexes().await.contains(&"x".to_owned()));
    assert_eq!(e.index_stats("x").await.unwrap(), None);
    assert_eq!(ids(&e, "y", vec![1.0, 0.0]).await, ["keep"]);
    // A later fold -- by either process -- brings none of it back.
    other.fold().await.unwrap();
    e.fold().await.unwrap();
    assert!(ids(&e, "x", vec![1.0, 0.0]).await.is_empty());
    assert!(ids(&other, "x", vec![1.0, 0.0]).await.is_empty());
    // And the other process, which flushed rows of it, stops reporting them on its next GET.
    assert_eq!(other.index_stats("x").await.unwrap(), None);
    assert!(!other.pending_indexes().await.contains(&"x".to_owned()));
}

#[tokio::test]
async fn rows_written_after_a_drop_create_the_index_again_with_a_new_schema() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(61), LaneId(1));
    let writer = Engine::new(Arc::clone(&store), TenantId(61), LaneId(2));
    e.write("x", vec![doc("a", vec![1.0, 0.0])]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    // A write-only process with BOTH door rungs stale at the drop (code review): it has read
    // the 2-wide schema, and it holds a flushed 2-wide row of its own.
    writer.indexes().await.unwrap();
    writer
        .write("x", vec![doc("w-old", vec![0.0, 1.0])])
        .await
        .unwrap();
    writer.flush().await.unwrap();
    e.delete_index("x").await.unwrap().expect("dropped");
    // New width, new metric.
    e.write_as(
        "x",
        vec![doc("b", vec![1.0, 2.0, 3.0])],
        Metric::EuclideanSquared,
    )
    .await
    .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let schema = e.index_stats("x").await.unwrap().unwrap().schema.unwrap();
    assert_eq!(
        (schema.client_dims(), schema.metric),
        (3, Metric::EuclideanSquared)
    );
    // The write-only process accepts the new width: its refusal re-read HEAD, remembered its
    // schema and pruned its rows.
    writer
        .write_as(
            "x",
            vec![doc("w-new", vec![0.0, 1.0, 0.0])],
            Metric::EuclideanSquared,
        )
        .await
        .expect("a stale door refused the recreated index's width");
    assert_eq!(ids(&e, "x", vec![1.0, 2.0, 3.0]).await, ["b"]);
}

#[tokio::test]
async fn as_of_before_a_drop_answers_with_the_old_metric() {
    // A retention that leaves the horizon at the queried epoch, short of every drop.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(62), LaneId(1));
    e.write_as(
        "x",
        vec![doc("near", vec![3.0, 0.1]), doc("far", vec![0.1, 3.0])],
        Metric::CosineDistance,
    )
    .await
    .unwrap();
    // Another index, dropped after the queried epoch and before x: its schema must not be
    // taken for x's (mutation sweep).
    e.write_as(
        "y",
        vec![doc("y", vec![1.0, 1.0])],
        Metric::EuclideanSquared,
    )
    .await
    .unwrap();
    e.flush().await.unwrap();
    let before = e.fold().await.unwrap();
    e.delete_index("y").await.unwrap().unwrap();
    e.delete_index("x").await.unwrap().unwrap();
    // Recreated under another metric and dropped again, then made once more: the present
    // schema is the wrong one, and so is the later drop's (code review).
    e.write_as(
        "x",
        vec![doc("mid", vec![1.0, 1.0])],
        Metric::EuclideanSquared,
    )
    .await
    .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.delete_index("x").await.unwrap().unwrap();
    e.write("x", vec![doc("new", vec![1.0, 1.0])])
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let head = e.head_for_test().await;
    e.gc(head.epoch.0 - before.0).await.unwrap();
    let answer = e
        .query_as_of(
            "x",
            before,
            &dense(vec![1.0, 0.0]),
            Fusion::Rrf { k: 60.0 },
            10,
        )
        .await
        .unwrap();
    let rows = e.resolve_rows(&answer);
    assert_eq!(rows[0].0, "near");
    // Cosine distance of [3, 0.1] from [1, 0]: 1 - 3/|[3, 0.1]|.
    let want = 1.0 - 3.0 / (9.0f32 + 0.01).sqrt();
    let got = rows[0].3.expect("a dense hit's $dist");
    assert!(
        (got - want).abs() < 1e-4,
        "{got} against {want}: not cosine"
    );
}

#[tokio::test]
async fn gc_directly_after_a_drop_reaps_it_and_its_dropped_schema() {
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(63), LaneId(1));
    e.write_as("x", vec![doc("a", vec![1.0, 0.0])], Metric::CosineDistance)
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.delete("x", vec!["a".to_owned()]).await.unwrap();
    e.write_as("x", vec![doc("b", vec![0.0, 1.0])], Metric::CosineDistance)
        .await
        .unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    let head = e.head_for_test().await;
    let mut keys: Vec<String> = head.indexes["x"].iter().map(|r| r.key.clone()).collect();
    keys.extend(head.deletes.values().map(|(k, _)| k.clone()));
    assert!(
        keys.iter().any(|k| k.ends_with(".dv")),
        "the fixture has a delete vector"
    );
    e.delete_index("x").await.unwrap().unwrap();
    assert_eq!(e.head_for_test().await.dropped.len(), 1);
    e.gc(0).await.unwrap();
    for k in &keys {
        assert!(
            store.get(&Key::new(k.clone())).await.is_err(),
            "{k} outlived its index"
        );
    }
    assert!(e.head_for_test().await.dropped.is_empty());
}

#[tokio::test]
async fn a_missing_index_is_not_deleted_and_two_deletes_answer_once() {
    let store = Arc::new(MemoryStore::new());
    let a = Engine::new(Arc::clone(&store), TenantId(64), LaneId(1));
    let b = Engine::new(Arc::clone(&store), TenantId(64), LaneId(2));
    a.write("x", vec![doc("r", vec![1.0])]).await.unwrap();
    a.flush().await.unwrap();
    a.fold().await.unwrap();
    let epoch = a.head_for_test().await.epoch;
    assert_eq!(a.delete_index("nope").await.unwrap(), None);
    assert_eq!(
        a.head_for_test().await.epoch,
        epoch,
        "a missing index committed"
    );
    // Both read the same HEAD; `b` commits first, `a` loses and re-decides on its retry.
    let ran = AtomicBool::new(false);
    let got = a
        .delete_index_with_interference_for_test("x", async {
            assert!(b.delete_index("x").await.unwrap().is_some());
            ran.store(true, Ordering::SeqCst);
        })
        .await
        .unwrap();
    assert!(ran.load(Ordering::SeqCst), "the other delete never ran");
    assert_eq!(
        got, None,
        "the losing delete answered as if it had dropped it"
    );
    assert_eq!(a.index_stats("x").await.unwrap(), None);
    // Pending rows alone make an index exist.
    a.write("p", vec![doc("r", vec![1.0])]).await.unwrap();
    assert!(a.delete_index("p").await.unwrap().is_some());
    assert!(!a.pending_indexes().await.contains(&"p".to_owned()));
}

#[tokio::test]
async fn an_index_only_a_bundle_holds_is_dropped_before_anything_is_sealed() {
    // Its rows leave the fold before the reject pass and the seal: no schema is created for
    // it, so none is kept as `dropped`, and no segment of it is written.
    let store = pstore_blob::Accounted::new(MemoryStore::new());
    let t = TenantId(66);
    let e = Engine::new(Arc::new(store.as_tenant(t)), t, LaneId(1));
    e.write("z", vec![doc("r", vec![1.0])]).await.unwrap();
    e.flush().await.unwrap();
    let writes = store.count(t, pstore_blob::OpClass::Write);
    assert!(e.delete_index("z").await.unwrap().is_some());
    let head = e.head_for_test().await;
    assert!(head.dropped.is_empty(), "{:?}", head.dropped);
    assert!(head.schemas.is_empty());
    assert_eq!(
        store.count(t, pstore_blob::OpClass::Write) - writes,
        1,
        "only the HEAD commit is written"
    );
}

#[tokio::test]
async fn an_index_head_names_only_by_its_rejects_can_be_dropped() {
    // Code review, round 2: one fold of a new index whose accepted row was deleted and whose
    // other row was rejected commits a reject count and no segment list. It exists, and the
    // drop clears the count, so a new index of that name does not inherit it.
    let store = Arc::new(MemoryStore::new());
    let a = Engine::new(Arc::clone(&store), TenantId(67), LaneId(1));
    let b = Engine::new(Arc::clone(&store), TenantId(67), LaneId(2));
    a.write("x", vec![doc("r", vec![1.0, 0.0])]).await.unwrap();
    a.delete("x", vec!["r".to_owned()]).await.unwrap();
    a.flush().await.unwrap();
    b.write("x", vec![doc("s", vec![1.0, 0.0, 0.0])])
        .await
        .unwrap();
    b.flush().await.unwrap();
    a.fold().await.unwrap();
    let head = a.head_for_test().await;
    assert!(
        !head.indexes.contains_key("x"),
        "the fixture sealed a segment"
    );
    assert_eq!(
        head.schema_rejects.get("x"),
        Some(&1),
        "the fixture counted no reject"
    );
    assert!(a.delete_index("x").await.unwrap().is_some());
    assert!(!a.head_for_test().await.schema_rejects.contains_key("x"));
}

// ---- A store that holds one bundle PUT, as `memtable_races.rs` does. ----

struct Held {
    inner: MemoryStore,
    armed: Mutex<bool>,
    arrived: Notify,
    release: Semaphore,
}

#[async_trait::async_trait]
impl BlobStore for Held {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        let hold =
            key.as_str().contains("/wal/") && std::mem::take(&mut *self.armed.lock().unwrap());
        if hold {
            self.arrived.notify_one();
            self.release.acquire().await.unwrap().forget();
        }
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

#[tokio::test]
async fn a_flush_in_flight_during_a_drop_loses_no_later_write() {
    let store = Arc::new(Held {
        inner: MemoryStore::new(),
        armed: Mutex::new(false),
        arrived: Notify::new(),
        release: Semaphore::new(0),
    });
    let e = Arc::new(Engine::new(Arc::clone(&store), TenantId(65), LaneId(1)));
    e.write("x", vec![doc("old", vec![1.0])]).await.unwrap();
    *store.armed.lock().unwrap() = true;
    let flushing = tokio::spawn({
        let e = Arc::clone(&e);
        async move { e.flush().await.unwrap() }
    });
    store.arrived.notified().await;
    let dropping = tokio::spawn({
        let e = Arc::clone(&e);
        async move { e.delete_index("x").await.unwrap() }
    });
    // Held behind the flush: the drop must not complete while the bundle PUT is in flight.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !dropping.is_finished(),
        "the drop ran past an in-flight flush"
    );
    store.release.add_permits(1);
    flushing.await.unwrap();
    let dropped: Option<Epoch> = dropping.await.unwrap();
    assert!(dropped.is_some());
    // A write after the drop is served, and survives a fold; the held bundle's row does not.
    e.write("x", vec![doc("new", vec![1.0])]).await.unwrap();
    assert_eq!(ids(&*e, "x", vec![1.0]).await, ["new"]);
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    assert_eq!(ids(&*e, "x", vec![1.0]).await, ["new"]);
}
