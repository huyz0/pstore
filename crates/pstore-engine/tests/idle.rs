//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M26: an engine is idle -- evictable -- only when it holds nothing a restart of its tenant
//! would lose. One test per kind of state, each isolating it: everything else is cleared, and
//! `count_reapable` is off unless it is the term under test.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_engine::{Engine, EngineError};
use pstore_format::Document;
use pstore_types::{CasTag, LaneId, TenantId};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const T: TenantId = TenantId(2600);

fn doc(id: &str) -> Document {
    Document::new(id, vec![1.0, 0.5])
}

/// The shared store, with switches for one engine's view of it.
#[derive(Debug, Clone)]
struct View {
    inner: Arc<MemoryStore>,
    /// Every HEAD commit answers `Contended`.
    contend_head: Arc<AtomicBool>,
    /// The next bundle write answers `Io`, having written nothing.
    fail_bundle: Arc<AtomicBool>,
}

impl View {
    fn over(inner: &Arc<MemoryStore>) -> Self {
        Self {
            inner: Arc::clone(inner),
            contend_head: Arc::default(),
            fail_bundle: Arc::default(),
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for View {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
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
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        if key.as_str().ends_with("/HEAD") && self.contend_head.load(Ordering::SeqCst) {
            return Err(CasError::Contended);
        }
        if key.as_str().ends_with(".bundle") && self.fail_bundle.swap(false, Ordering::SeqCst) {
            return Err(CasError::Io("refused before writing".to_owned()));
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

fn engine(view: &View, lane: u64) -> Engine<View> {
    Engine::new(Arc::new(view.clone()), T, LaneId(lane))
}

#[tokio::test]
async fn a_new_engine_and_a_settled_one_are_idle() {
    let store = Arc::new(MemoryStore::new());
    let e = engine(&View::over(&store), 1);
    assert!(e.is_idle(true), "a new engine is busy");
    e.write("idx", vec![doc("a")]).await.unwrap();
    e.flush().await.unwrap();
    e.fold().await.unwrap();
    // Folded: only the reapable commit is left.
    assert!(!e.is_idle(true), "a reapable commit counted for nothing");
    assert!(e.is_idle(false), "a settled engine is busy");
}

#[tokio::test]
async fn each_kind_of_state_keeps_an_engine_busy() {
    let store = Arc::new(MemoryStore::new());

    // Pending: acknowledged `batched`, not yet flushed.
    let e = engine(&View::over(&store), 1);
    e.write("idx", vec![doc("p")]).await.unwrap();
    assert!(!e.is_idle(false), "pending rows");

    // Durable: flushed, not yet folded.
    e.flush().await.unwrap();
    assert!(!e.is_idle(false), "flushed and unfolded rows");
    e.fold().await.unwrap();
    assert!(e.is_idle(false));

    // Abandoned: a fold whose every commit is refused leaves names to bury. Its rows were
    // written by another lane, so this engine holds nothing else.
    let other = engine(&View::over(&store), 9);
    other.write("idx", vec![doc("o")]).await.unwrap();
    other.flush().await.unwrap();
    let v = View::over(&store);
    let f = engine(&v, 2);
    v.contend_head.store(true, Ordering::SeqCst);
    f.fold()
        .await
        .expect_err("a fold committed through every refusal");
    assert!(!f.is_idle(false), "names left to bury");

    // An uncertain write: a bundle answered `Io`. A drop then takes its rows, so only the
    // record is left.
    let v = View::over(&store);
    let u = engine(&v, 3);
    u.write("tmp", vec![doc("u")]).await.unwrap();
    v.fail_bundle.store(true, Ordering::SeqCst);
    u.flush().await.expect_err("the bundle write did not fail");
    u.delete_index("tmp").await.unwrap();
    assert!(!u.is_idle(false), "an uncertain write");
}

#[tokio::test]
async fn a_taken_lane_keeps_an_engine_busy() {
    // A second writer on lane 1 takes the sequence A meant to write. A's rows then go with a
    // drop, so only the lane's refusal is left.
    let store = Arc::new(MemoryStore::new());
    let view = View::over(&store);
    let a = engine(&view, 1);
    let b = engine(&view, 1);
    a.write("idx", vec![doc("a1")]).await.unwrap();
    a.flush().await.unwrap();
    b.write("idx", vec![doc("b1")]).await.unwrap();
    b.flush().await.unwrap();
    a.write("tmp", vec![doc("a2")]).await.unwrap();
    let err = a.flush().await.expect_err("A overwrote B's bundle");
    assert!(matches!(err, EngineError::LaneTaken { .. }), "{err:?}");
    a.delete_index("tmp").await.unwrap();
    let reader = engine(&view, 9);
    reader.fold().await.unwrap();
    a.scan("idx", None).await.unwrap();
    assert!(!a.is_idle(false), "a lane known to be taken");
}
