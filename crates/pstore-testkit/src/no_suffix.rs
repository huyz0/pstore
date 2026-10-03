//! A store with no suffix reads, as Azure has none (C-14, M46).
//!
//! Two shapes, because the two questions differ:
//! - [`NoSuffix::new`] **says** it has none, as `ObjectStoreBackend::azure` does: what a
//!   segment open must work around.
//! - [`NoSuffix::claiming`] says it has them and fails them: what the conformance suite must
//!   catch rather than believe.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, Precondition, PutOutcome,
};
use pstore_types::CasTag;
use std::ops::Range;

/// Wraps `S` and refuses every suffix read.
#[derive(Debug, Clone)]
pub struct NoSuffix<S> {
    inner: S,
    caps: Capabilities,
}

impl<S: BlobStore> NoSuffix<S> {
    /// Declares `suffix_read: false`, honestly.
    #[must_use]
    pub fn new(inner: S) -> Self {
        let caps = Capabilities {
            suffix_read: false,
            ..inner.capabilities().clone()
        };
        Self { inner, caps }
    }

    /// Declares what `inner` declares, and fails suffix reads anyway.
    #[must_use]
    pub fn claiming(inner: S) -> Self {
        let caps = inner.capabilities().clone();
        Self { inner, caps }
    }

    fn refuse() -> BlobError {
        BlobError::Other("Operation not supported: no suffix range requests here".to_owned())
    }
}

#[async_trait::async_trait]
impl<S: BlobStore> BlobStore for NoSuffix<S> {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
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
    async fn get_suffix(&self, _: &Key, _: u64) -> Result<Bytes, BlobError> {
        Err(Self::refuse())
    }
    async fn get_suffix_as(&self, _: &Key, _: u64, _: Class) -> Result<Bytes, BlobError> {
        Err(Self::refuse())
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
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}
