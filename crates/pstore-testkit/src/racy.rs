//! A store whose conditional writes are flawed in the two ways the race probes look for (M279).
//!
//! - [`Flaw::CheckThenWrite`] evaluates a precondition, yields, then writes unconditionally: a
//!   single writer is fenced, and concurrent writers all win. The shape of a backend that
//!   checks a stale view.
//! - [`Flaw::DeletedCasIsAnError`] answers a compare-and-swap on a deleted object with an error
//!   rather than a conflict: what `object_store` does with S3's `404` today unless the adapter
//!   maps it.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, Precondition, PutOutcome,
};
use pstore_types::CasTag;
use std::ops::Range;

/// Which flaw a [`Racy`] has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flaw {
    /// Check, yield, write: concurrent writers all win.
    CheckThenWrite,
    /// A compare-and-swap on an absent object is an I/O error, not `Lost`.
    DeletedCasIsAnError,
}

/// Wraps `S` with one [`Flaw`] in `put_conditional`; everything else is forwarded.
#[derive(Debug, Clone)]
pub struct Racy<S> {
    inner: S,
    flaw: Flaw,
}

impl<S: BlobStore> Racy<S> {
    /// `inner`, with `flaw`.
    #[must_use]
    pub fn new(inner: S, flaw: Flaw) -> Self {
        Self { inner, flaw }
    }
}

#[async_trait::async_trait]
impl<S: BlobStore> BlobStore for Racy<S> {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        class: Class,
    ) -> Result<Bytes, BlobError> {
        self.inner.get_range_as(key, range, class).await
    }
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[Range<u64>],
        class: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        self.inner.get_ranges_as(key, ranges, class).await
    }
    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        self.inner.get_immutable(key, class).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.inner.get_suffix(key, n).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        self.inner.get_suffix_as(key, n, class).await
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
        match self.flaw {
            Flaw::CheckThenWrite => {
                let held = self
                    .inner
                    .get_tag(key)
                    .await
                    .map_err(|e| CasError::Io(e.to_string()))?;
                let holds = match (&pre, &held) {
                    (Precondition::NotExists, None) => true,
                    (Precondition::Match(want), Some(have)) => want == have,
                    _ => false,
                };
                if !holds {
                    return Err(CasError::Lost);
                }
                // The window: every racer has checked before any has written.
                tokio::task::yield_now().await;
                self.inner
                    .put(key, body)
                    .await
                    .map_err(|e| CasError::Io(e.to_string()))
            }
            Flaw::DeletedCasIsAnError => match (&pre, self.inner.get_tag(key).await) {
                (Precondition::Match(_), Ok(None)) => {
                    Err(CasError::Io("404 Not Found: NoSuchKey".to_owned()))
                }
                _ => self.inner.put_conditional(key, body, pre).await,
            },
        }
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}
