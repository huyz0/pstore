//! A backend that is *subtly* wrong in the specific ways real ones are.
//!
//! The conformance suite exists to detect a divergent backend, and until something is
//! deliberately divergent that detection is untested — the suite could be reporting
//! `Supported` unconditionally and every test would still pass. Each defect here is one a
//! real implementation actually has.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_types::CasTag;

/// A way for a backend to be wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Defect {
    /// `If-None-Match: *` is accepted and then ignored, so a second create succeeds.
    ///
    /// **MinIO does this**: it requires an exact ETag and rejects the wildcard, so two
    /// writers both believe they created the object.
    IgnoresCreateIfAbsent,
    /// A range that overruns the object returns the bytes that exist instead of failing.
    ///
    /// **HTTP permits this** (`206 Partial Content`), and a silent truncation surfaces
    /// layers away as a recall bug rather than as an error here.
    ShortReadsPastTheEnd,
    /// A suffix read returns the whole object.
    ///
    /// Functionally invisible, and costs a segment's bandwidth on every cold open.
    SuffixReturnsEverything,
    /// A deleted key still answers.
    IgnoresDeletes,
}

/// Wraps a correct backend and breaks one thing about it.
#[derive(Debug, Clone)]
pub struct Broken {
    inner: MemoryStore,
    defect: Defect,
}

impl Broken {
    /// A backend with exactly one defect.
    #[must_use]
    pub fn new(defect: Defect) -> Self {
        Self {
            inner: MemoryStore::new(),
            defect,
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for Broken {
    fn capabilities(&self) -> &Capabilities {
        // ⚠️ Still claims to be fine. That gap between declared and observed is the whole
        // reason capabilities are probed rather than read.
        self.inner.capabilities()
    }

    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        if self.defect == Defect::ShortReadsPastTheEnd {
            let body = self.inner.get(key).await?;
            let start = (range.start as usize).min(body.len());
            let end = (range.end as usize).min(body.len());
            return Ok(body.slice(start..end));
        }
        self.inner.get_range(key, range).await
    }

    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        if self.defect == Defect::SuffixReturnsEverything {
            return self.inner.get(key).await;
        }
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
        if self.defect == Defect::IgnoresCreateIfAbsent && pre == Precondition::NotExists {
            return self
                .inner
                .put(key, body)
                .await
                .map_err(|e| CasError::Io(e.to_string()));
        }
        self.inner.put_conditional(key, body, pre).await
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        if self.defect == Defect::IgnoresDeletes {
            return Ok(());
        }
        self.inner.delete_batch(keys).await
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
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

        if self.defect == Defect::ShortReadsPastTheEnd {
            let body = self.inner.get(key).await?;
            let start = (range.start as usize).min(body.len());
            let end = (range.end as usize).min(body.len());
            return Ok(body.slice(start..end));
        }
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

        if self.defect == Defect::SuffixReturnsEverything {
            return self.inner.get(key).await;
        }
        self.inner.get_suffix_as(key, n, class).await
    }

    async fn get_immutable(
        &self,
        key: &Key,
        class: pstore_blob::Class,
    ) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        self.inner.get_immutable(key, class).await
    }
}
