//! A store that holds writers at a barrier so a race is *forced* rather than hoped for.
//!
//! A test that spawns *n* tasks and trusts the scheduler to overlap them is not a
//! concurrency test, it is a lottery — and it fails the way lotteries do, occasionally and
//! for reasons that look like the code under test. This one blocks the first *n* commits
//! until all *n* have arrived, so every writer has done its work and is holding a
//! precondition from the same world before any of them lands.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_types::CasTag;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Barrier;

/// Wraps a store and releases the first `n` conditional writes together.
#[derive(Debug)]
pub struct Gated {
    inner: MemoryStore,
    barrier: Barrier,
    arrived: AtomicUsize,
    n: usize,
    armed: std::sync::atomic::AtomicBool,
}

impl Gated {
    /// A store whose first `n` conditional writes are released simultaneously.
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self {
            inner: MemoryStore::new(),
            barrier: Barrier::new(n),
            arrived: AtomicUsize::new(0),
            n,
            armed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Starts gating. Setting up a scenario takes writes of its own, and counting those
    /// against the barrier would release it before the racers ever arrive.
    pub fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl BlobStore for Gated {
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
    async fn get_tag(&self, key: &Key) -> Option<CasTag> {
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
        // ⚠️ Only the first `n`. A loser that rebases and retries must not re-enter the
        // barrier: it would wait for `n` peers that have already finished and deadlock the
        // test rather than fail it, which is the harder failure to read.
        if self.armed.load(Ordering::SeqCst) && self.arrived.fetch_add(1, Ordering::SeqCst) < self.n
        {
            self.barrier.wait().await;
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
