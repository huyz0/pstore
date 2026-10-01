//! A store that counts what the register asks of it, refuses chosen conditional writes as
//! `Contended`, and can hold each request at a gate a scheduler opens.

#![allow(dead_code, reason = "each test file uses a different part")]

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition, PutOutcome,
    TagStyle,
};
use pstore_types::CasTag;
use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Opens a gate for one request: the scheduler's half of an interleaving test.
#[async_trait::async_trait]
pub(crate) trait Gate: Send + Sync + std::fmt::Debug {
    /// Waits until `actor` may make its next request.
    async fn pass(&self, actor: usize);
}

#[derive(Debug, Default)]
pub(crate) struct Counts {
    pub(crate) reads: AtomicU64,
    pub(crate) cas: AtomicU64,
    pub(crate) puts: AtomicU64,
    pub(crate) lists: AtomicU64,
}

/// A view of one shared `MemoryStore`.
#[derive(Debug, Clone)]
pub(crate) struct Counting {
    pub(crate) inner: Arc<MemoryStore>,
    pub(crate) counts: Arc<Counts>,
    /// Ordinals of conditional writes refused as `Contended`.
    pub(crate) contend: Arc<Mutex<BTreeSet<u64>>>,
    /// Every read fails: a backend that is down.
    pub(crate) refuse_reads: Arc<std::sync::atomic::AtomicBool>,
    seen: Arc<AtomicU64>,
    gate: Option<(Arc<dyn Gate>, usize)>,
}

impl Counting {
    pub(crate) fn new() -> Self {
        Self::with_tags(TagStyle::Monotonic)
    }

    /// Over a store whose tags are content hashes, as S3's ETags are: ABA-prone.
    pub(crate) fn with_tags(style: TagStyle) -> Self {
        Self {
            inner: Arc::new(MemoryStore::with_tag_style(style)),
            counts: Arc::new(Counts::default()),
            contend: Arc::new(Mutex::new(BTreeSet::new())),
            refuse_reads: Arc::default(),
            seen: Arc::new(AtomicU64::new(0)),
            gate: None,
        }
    }

    /// This view, with every read and conditional write held at `gate` as `actor`.
    pub(crate) fn gated(&self, gate: Arc<dyn Gate>, actor: usize) -> Self {
        Self {
            gate: Some((gate, actor)),
            ..self.clone()
        }
    }

    /// The same store, counted apart: what a second tenant's view of it would bill.
    pub(crate) fn recounted(&self) -> Self {
        Self {
            counts: Arc::default(),
            ..self.clone()
        }
    }

    pub(crate) fn contend_at(&self, ordinals: &[u64]) {
        self.contend.lock().unwrap().extend(ordinals);
    }

    pub(crate) fn reads(&self) -> u64 {
        self.counts.reads.load(Ordering::SeqCst)
    }
    pub(crate) fn cas(&self) -> u64 {
        self.counts.cas.load(Ordering::SeqCst)
    }

    async fn pass(&self) {
        if let Some((g, a)) = &self.gate {
            g.pass(*a).await;
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for Counting {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.pass().await;
        self.counts.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.counts.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.counts.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.pass().await;
        if self.refuse_reads.load(Ordering::SeqCst) {
            return Err(BlobError::Other("injected read failure".to_owned()));
        }
        self.counts.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.counts.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.counts.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.counts.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.pass().await;
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        self.counts.cas.fetch_add(1, Ordering::SeqCst);
        if self.contend.lock().unwrap().contains(&n) {
            return Err(CasError::Contended);
        }
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.counts.lists.fetch_add(1, Ordering::SeqCst);
        self.inner.list_unrestricted(prefix).await
    }
}
