//! Measuring **sequential** blob depth, which is the number the round-trip budget is
//! denominated in.
//!
//! Counting requests is easy and wrong: a query issuing 200 ranged reads concurrently
//! costs one round trip, while one issuing 4 in a loop costs four. D-34 caps the second
//! number at three and says nothing about the first, so the counter has to tell them
//! apart.
//!
//! The rule: **a new round begins whenever a request starts while nothing is in flight.**
//! Fan-out issued before anything completes counts once; a loop that awaits each result
//! before issuing the next counts every iteration.

use bytes::Bytes;
use pstore_blob::{BlobError, Capabilities, CasError, Key, Precondition, PutOutcome};
use pstore_types::CasTag;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Default)]
struct Counters {
    in_flight: AtomicUsize,
    depth: AtomicUsize,
    requests: AtomicUsize,
}

/// Wraps a backend and records sequential depth.
#[derive(Debug, Clone)]
pub struct DepthCounting<S> {
    inner: Arc<S>,
    c: Arc<Counters>,
}

/// Held for the life of one request, so depth is recorded even if the request fails.
struct Guard(Arc<Counters>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<S: pstore_blob::BlobStore> DepthCounting<S> {
    /// Wraps `inner`.
    pub fn new(inner: S) -> Self {
        Self {
            inner: Arc::new(inner),
            c: Arc::new(Counters::default()),
        }
    }

    /// The store beneath, for a test that needs to see what the decorator passed down.
    #[must_use]
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Sequential rounds observed since [`Self::reset`].
    #[must_use]
    pub fn depth(&self) -> usize {
        self.c.depth.load(Ordering::SeqCst)
    }

    /// Total requests, however issued. Depth's denominator, and the cost figure.
    #[must_use]
    pub fn requests(&self) -> usize {
        self.c.requests.load(Ordering::SeqCst)
    }

    /// Clears both counters, so a test can measure one operation rather than a session.
    pub fn reset(&self) {
        self.c.depth.store(0, Ordering::SeqCst);
        self.c.requests.store(0, Ordering::SeqCst);
    }

    fn begin(&self) -> Guard {
        self.c.requests.fetch_add(1, Ordering::SeqCst);
        // `fetch_add` returns the PREVIOUS value: zero means nothing was in flight, so
        // this request could not have been issued concurrently with another.
        if self.c.in_flight.fetch_add(1, Ordering::SeqCst) == 0 {
            self.c.depth.fetch_add(1, Ordering::SeqCst);
        }
        Guard(Arc::clone(&self.c))
    }

    /// Registers the request, then yields.
    ///
    /// ⚠️ The yield is the point. Against a synchronous backend every future completes
    /// the instant it is first polled, so `join_all` would run them one at a time and
    /// genuine fan-out would be indistinguishable from a loop. Yielding lets every
    /// concurrently-polled request register before any completes, which is what a backend
    /// with a network in front of it does anyway.
    async fn begin_async(&self) -> Guard {
        let g = self.begin();
        tokio::task::yield_now().await;
        g
    }
}

#[async_trait::async_trait]
impl<S: pstore_blob::BlobStore> pstore_blob::BlobStore for DepthCounting<S> {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }

    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        let _g = self.begin_async().await;
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        let _g = self.begin_async().await;
        self.inner.get_range(key, range).await
    }

    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        let _g = self.begin_async().await;
        self.inner.get_with_tag(key).await
    }

    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        let _g = self.begin_async().await;
        self.inner.get_suffix(key, n).await
    }

    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        let _g = self.begin_async().await;
        self.inner.get_tag(key).await
    }

    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        let _g = self.begin_async().await;
        self.inner.head(key).await
    }

    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        let _g = self.begin_async().await;
        self.inner.put(key, body).await
    }

    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        let _g = self.begin_async().await;
        self.inner.put_conditional(key, body, pre).await
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        let _g = self.begin_async().await;
        self.inner.delete_batch(keys).await
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        let _g = self.begin_async().await;
        self.inner.list_unrestricted(prefix).await
    }

    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: pstore_blob::Class,
    ) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        let _g = self.begin_async().await;
        self.inner.get_range_as(key, range, class).await
    }

    async fn get_suffix_as(
        &self,
        key: &Key,
        n: u64,
        class: pstore_blob::Class,
    ) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        let _g = self.begin_async().await;
        self.inner.get_suffix_as(key, n, class).await
    }

    async fn get_immutable(
        &self,
        key: &Key,
        class: pstore_blob::Class,
    ) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        let _g = self.begin_async().await;
        self.inner.get_immutable(key, class).await
    }
}
