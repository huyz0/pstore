//! A store that **refuses** to let Invariant I1 be broken.
//!
//! > *No node ever mutates an object another node might read, and every state transition
//! > is a CAS on a single key conditioned on the exact version the actor observed.*
//!
//! Stated in prose, that is a hope. Here it is a predicate: an unconditional write to a key
//! that already exists is a mutation of an object a reader may be holding, and the auditor
//! fails the run rather than recording a note nobody reads. Everything else in the design —
//! snapshot isolation, fencing without locks, time travel, branching — rests on it, so it
//! is worth a gate rather than a comment.

use bytes::Bytes;
use pstore_blob::{BlobError, BlobStore, Capabilities, CasError, Key, Precondition, PutOutcome};
use pstore_types::CasTag;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

/// An observed breach of Invariant I1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The key that was mutated.
    pub key: String,
    /// What the writer did.
    pub what: &'static str,
}

#[derive(Debug, Default)]
struct Seen {
    written: BTreeSet<String>,
    violations: Vec<Violation>,
}

/// Wraps a backend and watches for in-place mutation.
#[derive(Debug, Clone)]
pub struct Auditing<S> {
    inner: Arc<S>,
    seen: Arc<Mutex<Seen>>,
}

impl<S: BlobStore> Auditing<S> {
    /// Wraps `inner`.
    pub fn new(inner: S) -> Self {
        Self {
            inner: Arc::new(inner),
            seen: Arc::new(Mutex::new(Seen::default())),
        }
    }

    /// Every violation observed. **Empty is the only passing result.**
    #[must_use]
    pub fn violations(&self) -> Vec<Violation> {
        self.lock().violations.clone()
    }

    /// Distinct keys written. The denominator: an audit over no writes proves nothing, so
    /// a test should assert this is non-trivial before trusting an empty violation list.
    #[must_use]
    pub fn keys_written(&self) -> usize {
        self.lock().written.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl<S: BlobStore> BlobStore for Auditing<S> {
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
        {
            let mut s = self.lock();
            // ⚠️ The whole audit. An unconditional PUT over a key that exists overwrites
            // bytes a reader may be mid-fetch on, and does so without naming the version it
            // believed it was replacing — which is exactly what CAS exists to prevent.
            if !s.written.insert(key.as_str().to_owned()) {
                s.violations.push(Violation {
                    key: key.as_str().to_owned(),
                    what: "unconditional overwrite of an existing key",
                });
            }
        }
        self.inner.put(key, body).await
    }

    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        // Legitimate by construction: the write names the version it replaces, so a writer
        // holding a stale view is refused by the store rather than by our vigilance.
        self.lock().written.insert(key.as_str().to_owned());
        self.inner.put_conditional(key, body, pre).await
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        // A reaped key may legitimately be written again later — GC is what makes a key
        // free — so forget it rather than treating the next write as a mutation.
        {
            let mut s = self.lock();
            for k in keys {
                s.written.remove(k.as_str());
            }
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
