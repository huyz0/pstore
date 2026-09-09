//! A correct store that **reports** a chosen capability profile.
//!
//! ⚠️ The exact inverse of [`broken`](crate::broken), and the pair is deliberate. `Broken`
//! misbehaves while claiming to be fine — which is what the conformance suite exists to
//! catch. This one behaves perfectly while claiming *not* to be fine, which is what a policy
//! that reads a recorded profile has to act on.
//!
//! Correctness of the underlying store is the point. If it also misbehaved, a test could pass
//! because the operation failed rather than because it was refused — and "refused before
//! anything was written" is the property, not "failed somehow".

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition, PutOutcome,
    Support,
};
use pstore_types::CasTag;

/// Wraps a correct backend and overrides the profile it reports.
#[derive(Debug)]
pub struct Claims {
    inner: MemoryStore,
    caps: Capabilities,
}

impl Claims {
    /// A store reporting exactly `caps` and behaving correctly regardless.
    #[must_use]
    pub fn new(caps: Capabilities) -> Self {
        Self::over(MemoryStore::new(), caps)
    }

    /// The same objects, reported under a different profile.
    ///
    /// ⚠️ `MemoryStore` clones share their state, so this is a **re-probe**: the world is
    /// exactly as some conforming writer left it and the profile has since changed. That is
    /// the realistic shape — a backend upgraded under us, or `conformance.sh --check` finding
    /// a divergence — and it is the only way to hand a refused operation something real to
    /// have destroyed, which is what the request counters are there to catch.
    #[must_use]
    pub fn over(inner: MemoryStore, caps: Capabilities) -> Self {
        Self { inner, caps }
    }

    /// The `MemoryStore` beneath, for handing to a differently-profiled `Claims`.
    #[must_use]
    pub fn store(&self) -> MemoryStore {
        self.inner.clone()
    }

    /// A store whose profile says its `cas` is present and wrong — MinIO's shape.
    #[must_use]
    pub fn divergent_cas(note: &str) -> Self {
        Self::with(Support::Divergent(note.to_owned()), Support::Supported)
    }

    /// A store whose profile says `create_if_absent` is present and wrong.
    #[must_use]
    pub fn divergent_create(note: &str) -> Self {
        Self::with(Support::Supported, Support::Divergent(note.to_owned()))
    }

    /// A store whose profile is clean.
    #[must_use]
    pub fn conforming() -> Self {
        Self::with(Support::Supported, Support::Supported)
    }

    /// The profile a divergent re-probe would record, over `inner`'s objects.
    #[must_use]
    pub fn degraded(inner: MemoryStore, note: &str) -> Self {
        Self::over(
            inner,
            Self::caps(Support::Divergent(note.to_owned()), Support::Supported),
        )
    }

    fn with(cas: Support, create_if_absent: Support) -> Self {
        Self::new(Self::caps(cas, create_if_absent))
    }

    fn caps(cas: Support, create_if_absent: Support) -> Capabilities {
        Capabilities {
            backend: "emulator-under-test".to_owned(),
            cas,
            create_if_absent,
            delete_is_free: true,
            max_batch_delete: 1000,
            coalesce_gap: 0,
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for Claims {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
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
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}
