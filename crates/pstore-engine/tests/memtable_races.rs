//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The memtable's races — M9c.1, BACKLOG rows 35, 36 and 38. Each window is FORCED: a store
//! holds one write until the test releases it, so the interleaving is the one the test names.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::Engine;
use pstore_format::Document;
use pstore_index::vec_index::Query;
use pstore_query::{Fusion, Prefetch};
use pstore_types::{CasTag, LaneId, TenantId};
use std::ops::Range;
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, Semaphore};

/// Which write to hold.
#[derive(Clone, Copy, PartialEq)]
enum Hold {
    /// A bundle PUT: a flush.
    Bundle,
    /// A HEAD compare-and-swap: a fold's commit.
    Commit,
}

/// A `MemoryStore` that parks the first write of the armed kind until released.
struct Held {
    inner: MemoryStore,
    armed: Mutex<Option<Hold>>,
    /// Signalled when the held write arrives.
    arrived: Notify,
    /// A permit lets it through.
    release: Semaphore,
}

impl Held {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(),
            armed: Mutex::new(None),
            arrived: Notify::new(),
            release: Semaphore::new(0),
        })
    }

    fn arm(&self, what: Hold) {
        *self.armed.lock().unwrap() = Some(what);
    }

    async fn hold(&self, what: Hold) {
        let hit = {
            let mut a = self.armed.lock().unwrap();
            if *a == Some(what) {
                *a = None;
                true
            } else {
                false
            }
        };
        if hit {
            self.arrived.notify_one();
            self.release.acquire().await.unwrap().forget();
        }
    }
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
        if key.as_str().contains("/wal/") {
            self.hold(Hold::Bundle).await;
        }
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        if key.as_str().ends_with("/HEAD") {
            self.hold(Hold::Commit).await;
        }
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

fn doc(id: &str) -> Document {
    Document::new(id, vec![1.0, 0.5])
}

/// Every id a broad query returns, sorted, duplicates kept.
async fn ids<S: BlobStore>(e: &Engine<S>) -> Vec<String> {
    let legs = vec![Prefetch::Dense {
        field: pstore_format::DEFAULT_FIELD.to_owned(),
        query: vec![1.0, 0.5],
        limit: 100,
        tune: Query::default(),
    }];
    let answer = e
        .query("idx", &legs, Fusion::Rrf { k: 60.0 }, 100)
        .await
        .unwrap();
    let mut out: Vec<String> = e.resolve(&answer).into_iter().map(|(id, _)| id).collect();
    out.sort();
    out
}

#[tokio::test]
async fn rows_stay_visible_while_their_flush_is_in_flight() {
    // Row 35: the flush took the rows out of `pending` before its PUT and put them in
    // `durable` after, so a query in between saw neither.
    let store = Held::new();
    let e = Arc::new(Engine::new(Arc::clone(&store), TenantId(1), LaneId(1)));
    e.write("idx", vec![doc("a")]).await.unwrap();
    store.arm(Hold::Bundle);
    let flushing = tokio::spawn({
        let e = Arc::clone(&e);
        async move { e.flush().await.unwrap() }
    });
    store.arrived.notified().await;
    assert_eq!(
        ids(&e).await,
        ["a"],
        "an acknowledged row vanished during its flush"
    );
    // ⚠️ A row written WHILE the PUT is in flight is not in this bundle, and must not be filed
    // under it (review of M9c.1): the next fold would prune it with the bundle, and it would
    // be gone for good.
    e.write("idx", vec![doc("b")]).await.unwrap();
    store.release.add_permits(1);
    flushing.await.unwrap();
    assert_eq!(ids(&e).await, ["a", "b"]);
    assert_eq!(
        e.pending_for_test().await,
        ["b"],
        "a row the bundle lacks was marked flushed"
    );
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    assert_eq!(
        ids(&e).await,
        ["a", "b"],
        "a row written during a flush was lost at the fold"
    );
}

#[tokio::test]
async fn a_fold_keeps_rows_flushed_after_it_read_the_lane() {
    // Row 36: the fold reads bundle 0, and before it commits a second flush lands bundle 1.
    // Clearing all of `durable` at the commit hid bundle 1's rows until the next fold.
    let store = Held::new();
    let e = Arc::new(Engine::new(Arc::clone(&store), TenantId(2), LaneId(1)));
    e.write("idx", vec![doc("a")]).await.unwrap();
    e.flush().await.unwrap();
    store.arm(Hold::Commit);
    let folding = tokio::spawn({
        let e = Arc::clone(&e);
        async move { e.fold().await.unwrap() }
    });
    store.arrived.notified().await;
    e.write("idx", vec![doc("b")]).await.unwrap();
    e.flush().await.unwrap();
    store.release.add_permits(1);
    folding.await.unwrap();
    assert_eq!(
        ids(&e).await,
        ["a", "b"],
        "a durable row was hidden by a fold that never read it"
    );
}

#[tokio::test]
async fn rows_another_process_folded_are_served_once() {
    // Row 38: this engine's flushed rows, folded by ANOTHER engine on another lane, were
    // still in this one's `durable` -- and returned twice, once from the segment.
    let store = Arc::new(MemoryStore::new());
    let writer = Engine::new(Arc::clone(&store), TenantId(3), LaneId(1));
    let folder = Engine::new(Arc::clone(&store), TenantId(3), LaneId(2));
    writer.write("idx", vec![doc("a"), doc("b")]).await.unwrap();
    writer.flush().await.unwrap();
    folder.fold().await.unwrap();
    assert_eq!(
        ids(&writer).await,
        ["a", "b"],
        "folded rows were served twice"
    );
    assert_eq!(ids(&folder).await, ["a", "b"]);
    // And rows written after the other fold are still this engine's to serve.
    writer.write("idx", vec![doc("c")]).await.unwrap();
    assert_eq!(ids(&writer).await, ["a", "b", "c"]);
}

#[tokio::test]
async fn a_refused_flush_leaves_its_rows_where_they_were() {
    // The schema refusal no longer takes the rows out and puts them back: it never moves them.
    let store = Arc::new(MemoryStore::new());
    let e = Engine::new(Arc::clone(&store), TenantId(4), LaneId(1));
    e.write("idx", vec![doc("a")]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    e.write_without_schema_check_for_test("idx", vec![Document::new("wide", vec![1.0, 2.0, 3.0])])
        .await;
    assert!(
        e.flush().await.is_err(),
        "a width the schema refuses was flushed"
    );
    assert_eq!(e.pending_for_test().await, ["wide"]);
}
