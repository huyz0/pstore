//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! M86: a registration or a drop that meets a contended HEAD retries, lands, and gives up with
//! `Contended` -- not `Lost` -- after exactly the attempts every commit loop here makes.

use pstore_blob::{BlobStore, Key, MemoryStore};
use pstore_engine::{Engine, EngineError, ExternalKind, ExternalRefusal};
use pstore_types::{LaneId, TenantId};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

const T: TenantId = TenantId(86);
/// `MAX_COMMIT_ATTEMPTS`, in `lib.rs`.
const ATTEMPTS: u32 = 24;

/// A store whose HEAD commits answer `Contended` while `left` > 0, counting them.
#[derive(Debug)]
struct Contend {
    inner: MemoryStore,
    left: AtomicU32,
    refused: AtomicU32,
    /// A rival's HEAD, landed by the next HEAD commit in place of ours, which then loses.
    rival: std::sync::Mutex<Option<bytes::Bytes>>,
}

impl Contend {
    fn new(times: u32) -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(),
            left: times.into(),
            refused: 0.into(),
            rival: std::sync::Mutex::default(),
        })
    }
}

#[async_trait::async_trait]
impl BlobStore for Contend {
    fn capabilities(&self) -> &pstore_blob::Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
    ) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<bytes::Bytes, pstore_blob::BlobError> {
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(
        &self,
        key: &Key,
    ) -> Result<(bytes::Bytes, pstore_types::CasTag), pstore_blob::BlobError> {
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(
        &self,
        key: &Key,
    ) -> Result<Option<pstore_types::CasTag>, pstore_blob::BlobError> {
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, pstore_blob::BlobError> {
        self.inner.head(key).await
    }
    async fn put(
        &self,
        key: &Key,
        body: bytes::Bytes,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::BlobError> {
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: bytes::Bytes,
        pre: pstore_blob::Precondition,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::CasError> {
        if key.as_str().ends_with("/HEAD")
            && self
                .left
                .fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            self.refused.fetch_add(1, SeqCst);
            return Err(pstore_blob::CasError::Contended);
        }
        let rival = self.rival.lock().unwrap().take();
        if key.as_str().ends_with("/HEAD")
            && let Some(theirs) = rival
        {
            self.inner.put(key, theirs).await.unwrap();
            return Err(pstore_blob::CasError::Lost);
        }
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), pstore_blob::BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, pstore_blob::BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

async fn engine(times: u32) -> (Arc<Contend>, Engine<Contend>) {
    let store = Contend::new(times);
    let raw = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../pstore-format/tests/data/good.parquet"
    ))
    .unwrap();
    store
        .put(&Key::new("lake/good.parquet"), raw.into())
        .await
        .unwrap();
    let e = Engine::new(Arc::clone(&store), T, LaneId(1))
        .with_external_sources(vec!["lake/".to_owned()]);
    (store, e)
}

async fn register(e: &Engine<Contend>) -> Result<pstore_types::Epoch, EngineError> {
    e.register_external("x", ExternalKind::Parquet, "lake/good.parquet", "embedding")
        .await
}

#[tokio::test(start_paused = true)]
async fn a_contended_registration_retries_and_lands() {
    let (store, e) = engine(3).await;
    register(&e).await.unwrap();
    assert_eq!(store.refused.load(SeqCst), 3);
    assert!(e.head_for_test().await.externals.contains_key("x"));
}

#[tokio::test(start_paused = true)]
async fn a_registration_that_keeps_contending_reports_contention_after_every_attempt() {
    let (store, e) = engine(u32::MAX).await;
    let err = register(&e).await.unwrap_err();
    assert!(matches!(err, EngineError::Contended), "{err:?}");
    // The registration's attempts, and the same again for the commit that buries the codes it
    // built (which meets the same contention and gives up the same way).
    assert_eq!(store.refused.load(SeqCst), 2 * ATTEMPTS);
    // Nothing is left naming it.
    store.left.store(0, SeqCst);
    assert!(e.head_for_test().await.externals.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_contended_drop_retries_and_lands() {
    let (store, e) = engine(0).await;
    register(&e).await.unwrap();
    store.left.store(3, SeqCst);
    assert!(e.drop_external("x").await.unwrap().is_some());
    assert_eq!(store.refused.load(SeqCst), 3);
    assert!(e.head_for_test().await.externals.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_drop_that_keeps_contending_reports_contention_after_every_attempt() {
    let (store, e) = engine(0).await;
    register(&e).await.unwrap();
    store.left.store(u32::MAX, SeqCst);
    let err = e.drop_external("x").await.unwrap_err();
    assert!(matches!(err, EngineError::Contended), "{err:?}");
    assert_eq!(store.refused.load(SeqCst), ATTEMPTS);
    store.left.store(0, SeqCst);
    assert!(e.head_for_test().await.externals.contains_key("x"));
}

#[tokio::test(start_paused = true)]
async fn a_registration_that_loses_to_a_rival_of_the_same_name_is_refused() {
    // Two registrations of one name race: the one that loses re-reads HEAD, finds the name
    // taken, and refuses rather than retrying over the rival's index.
    let (store, e) = engine(0).await;
    register(&e).await.unwrap();
    let rivals = format!("{:?}", e.head_for_test().await.externals["x"]);
    let keys = store.inner.list_unrestricted(&Key::new("")).await.unwrap();
    let head = keys
        .into_iter()
        .find(|k| k.as_str().ends_with("/HEAD"))
        .unwrap();
    // Taken back out, so the next registration sees the name free, builds, and commits.
    let theirs = store.inner.get(&head).await.unwrap();
    store
        .inner
        .delete_batch(std::slice::from_ref(&head))
        .await
        .unwrap();
    *store.rival.lock().unwrap() = Some(theirs.clone());

    let fresh = Engine::new(Arc::clone(&store), T, LaneId(1))
        .with_external_sources(vec!["lake/".to_owned()]);
    let err = register(&fresh).await.unwrap_err();
    assert!(
        matches!(err, EngineError::External(ExternalRefusal::Exists(ref n)) if n == "x"),
        "{err:?}"
    );
    // The rival's index stands, and nothing else was registered.
    let h = fresh.head_for_test().await;
    assert_eq!(format!("{:?}", h.externals["x"]), rivals);
    assert_eq!(h.externals.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_registration_that_loses_to_an_unrelated_commit_retries_and_lands() {
    let (store, e) = engine(0).await;
    // Any HEAD that names nothing new: the rival changed something else.
    let a = Engine::new(Arc::clone(&store), T, LaneId(2))
        .with_external_sources(vec!["lake/".to_owned()]);
    a.register_external("y", ExternalKind::Parquet, "lake/good.parquet", "embedding")
        .await
        .unwrap();
    let keys = store.inner.list_unrestricted(&Key::new("")).await.unwrap();
    let head = keys
        .into_iter()
        .find(|k| k.as_str().ends_with("/HEAD"))
        .unwrap();
    *store.rival.lock().unwrap() = Some(store.inner.get(&head).await.unwrap());
    register(&e).await.unwrap();
    let h = e.head_for_test().await;
    assert!(h.externals.contains_key("x") && h.externals.contains_key("y"));
}

#[tokio::test]
async fn an_invalid_name_is_refused_before_anything_is_read() {
    let (_, e) = engine(0).await;
    let err = e
        .register_external(
            "not/a name",
            ExternalKind::Parquet,
            "lake/good.parquet",
            "embedding",
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::Refused(ref m) if m.contains("not a valid index name")),
        "{err:?}"
    );
}
