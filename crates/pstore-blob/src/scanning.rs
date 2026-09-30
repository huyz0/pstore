//! The scan decorator (M20).

use crate::{BlobStore, Key};
use std::sync::Arc;

/// A store, for a pass that reads every block of what it touches once: a compaction, a
/// fold's delete and upsert pass, a full scan (M20).
///
/// ⚠️ **Re-classes bulk reads only**, as [`Class::Scan`], which a
/// read cache serves but never admits (D-50). A segment's footer and sidecars keep their
/// class: they are small, and a query reads them again. Every read form is overridden: the
/// trait's default `get_ranges` coalesces into a classless `get_range`, which would reach the
/// cache as bulk.
#[derive(Debug)]
pub struct Scanning<S>(Arc<S>);

impl<S> Scanning<S> {
    /// `inner`, with its bulk reads re-classed as scans.
    #[must_use]
    pub fn new(inner: Arc<S>) -> Self {
        Self(inner)
    }
}

fn scan_class(class: crate::Class) -> crate::Class {
    match class {
        crate::Class::Bulk => crate::Class::Scan,
        other => other,
    }
}

#[async_trait::async_trait]
impl<S: BlobStore> BlobStore for Scanning<S> {
    fn capabilities(&self) -> &crate::Capabilities {
        self.0.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<bytes::Bytes, crate::BlobError> {
        self.0.get(key).await
    }
    async fn get_range(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
    ) -> Result<bytes::Bytes, crate::BlobError> {
        self.0.get_range_as(key, range, crate::Class::Scan).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: crate::Class,
    ) -> Result<bytes::Bytes, crate::BlobError> {
        self.0.get_range_as(key, range, scan_class(class)).await
    }
    async fn get_ranges(
        &self,
        key: &Key,
        ranges: &[std::ops::Range<u64>],
    ) -> Result<Vec<bytes::Bytes>, crate::BlobError> {
        self.0.get_ranges_as(key, ranges, crate::Class::Scan).await
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[std::ops::Range<u64>],
        class: crate::Class,
    ) -> Result<Vec<bytes::Bytes>, crate::BlobError> {
        self.0.get_ranges_as(key, ranges, scan_class(class)).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<bytes::Bytes, crate::BlobError> {
        self.0.get_suffix_as(key, n, crate::Class::Scan).await
    }
    async fn get_suffix_as(
        &self,
        key: &Key,
        n: u64,
        class: crate::Class,
    ) -> Result<bytes::Bytes, crate::BlobError> {
        self.0.get_suffix_as(key, n, scan_class(class)).await
    }
    async fn get_immutable(
        &self,
        key: &Key,
        class: crate::Class,
    ) -> Result<bytes::Bytes, crate::BlobError> {
        self.0.get_immutable(key, scan_class(class)).await
    }
    async fn get_with_tag(
        &self,
        key: &Key,
    ) -> Result<(bytes::Bytes, pstore_types::CasTag), crate::BlobError> {
        self.0.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<pstore_types::CasTag>, crate::BlobError> {
        self.0.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, crate::BlobError> {
        self.0.head(key).await
    }
    async fn put(
        &self,
        key: &Key,
        body: bytes::Bytes,
    ) -> Result<crate::PutOutcome, crate::BlobError> {
        self.0.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: bytes::Bytes,
        pre: crate::Precondition,
    ) -> Result<crate::PutOutcome, crate::CasError> {
        self.0.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), crate::BlobError> {
        self.0.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, crate::BlobError> {
        self.0.list_unrestricted(prefix).await
    }
}
