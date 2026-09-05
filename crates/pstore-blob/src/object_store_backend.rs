//! Adapter onto the `object_store` crate, and therefore onto S3, GCS and Azure.
//!
//! ⚠️ **Integration is unverified.** No cloud accounts, and no emulator running in CI, so
//! this compiles and is exercised against the in-memory backend only. Its purpose in M0a
//! is to answer the milestone's first risk — *is the trait shaped for a real backend?* —
//! by writing the adapter rather than assuming. What it revealed is recorded in
//! `docs/milestones/M0a/VERIFIED.md`.

use crate::{BlobError, Capabilities, CasError, Key, Precondition, PutOutcome, Support};
use bytes::Bytes;
// `ObjectStoreExt` is not decoration: `get`, `get_range` and `head` live there rather
// than on the base trait, so `Arc<dyn ObjectStore>` alone does not have them.
use object_store::{
    Attributes, ObjectStore, ObjectStoreExt, PutMode, PutOptions, TagSet, UpdateVersion, path::Path,
};
use pstore_types::CasTag;
use std::sync::Arc;

/// Wraps any `object_store` implementation.
#[derive(Debug, Clone)]
pub struct ObjectStoreBackend {
    inner: Arc<dyn ObjectStore>,
    caps: Capabilities,
}

impl ObjectStoreBackend {
    /// Wraps `inner` with a capability profile that **came from the conformance suite**.
    ///
    /// Not from what the backend claims: MinIO advertises conditional writes and then
    /// ignores the `If-None-Match: *` wildcard, so a hand-set profile is a statement of
    /// hope. See [`Self::unprobed`] for the pre-measurement value.
    #[must_use]
    pub fn new(inner: Arc<dyn ObjectStore>, caps: Capabilities) -> Self {
        Self { inner, caps }
    }

    /// A profile for a backend the conformance suite has not run against yet.
    ///
    /// Deliberately **not** `Supported`: a backend is divergent until measured, and one
    /// recorded this way must refuse `durable` writes rather than corrupt silently.
    #[must_use]
    pub fn unprobed(backend: impl Into<String>) -> Capabilities {
        Capabilities {
            backend: backend.into(),
            cas: Support::Divergent("not yet probed by the conformance suite".to_owned()),
            create_if_absent: Support::Divergent("not yet probed".to_owned()),
            delete_is_free: false,
            max_batch_delete: 1000,
            coalesce_gap: 1024 * 1024,
        }
    }

    fn path(key: &Key) -> Path {
        Path::from(key.as_str())
    }

    fn map_err(e: object_store::Error) -> BlobError {
        match e {
            object_store::Error::NotFound { .. } => BlobError::NotFound(e.to_string()),
            other => {
                // `object_store` surfaces 503 SlowDown only in the message, which is
                // itself a finding: the signal our congestion controller needs is not a
                // typed variant. Recorded for M0b to confirm against a real throttle.
                let s = other.to_string();
                if s.contains("SlowDown") || s.contains("503") {
                    BlobError::SlowDown
                } else {
                    BlobError::Other(s)
                }
            }
        }
    }

    fn put_opts(pre: &Precondition) -> PutOptions {
        PutOptions {
            mode: match pre {
                Precondition::NotExists => PutMode::Create,
                Precondition::Match(tag) => PutMode::Update(UpdateVersion {
                    e_tag: Some(tag.as_str().to_owned()),
                    version: None,
                }),
            },
            tags: TagSet::default(),
            attributes: Attributes::default(),
            extensions: object_store::Extensions::default(),
        }
    }
}

#[async_trait::async_trait]
impl crate::BlobStore for ObjectStoreBackend {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner
            .get(&Self::path(key))
            .await
            .map_err(Self::map_err)?
            .bytes()
            .await
            .map_err(Self::map_err)
    }

    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        let want = range.end.saturating_sub(range.start);
        let got = self
            .inner
            .get_range(&Self::path(key), range.clone())
            .await
            .map_err(Self::map_err)?;
        // ⚠️ FOUND BY THE CONFORMANCE SUITE, not by reading the docs. HTTP `Range` is
        // permitted to answer a range that overruns the object with `206 Partial
        // Content` and the bytes that exist -- a SHORT READ, not an error -- and
        // `object_store` passes that through. Our trait promises the requested bytes or
        // an error, because a silent truncation here surfaces several layers away as a
        // recall bug: a posting list that ends early scores as a missing document.
        if got.len() as u64 != want {
            return Err(BlobError::RangeOutOfBounds(range, got.len() as u64));
        }
        Ok(got)
    }

    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        // One GET: the response carries the ETag, so the pair is atomic by construction
        // rather than by luck.
        let r = self
            .inner
            .get(&Self::path(key))
            .await
            .map_err(Self::map_err)?;
        let tag = CasTag::new(r.meta.e_tag.clone().unwrap_or_default());
        Ok((r.bytes().await.map_err(Self::map_err)?, tag))
    }

    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        // `GetRange::Suffix` is `Range: bytes=-N` on the wire. No `head` first.
        let opts = object_store::GetOptions {
            range: Some(object_store::GetRange::Suffix(n)),
            ..Default::default()
        };
        self.inner
            .get_opts(&Self::path(key), opts)
            .await
            .map_err(Self::map_err)?
            .bytes()
            .await
            .map_err(Self::map_err)
    }

    async fn get_tag(&self, key: &Key) -> Option<CasTag> {
        self.inner
            .head(&Self::path(key))
            .await
            .ok()
            .and_then(|m| m.e_tag)
            .map(CasTag::new)
    }

    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.inner
            .head(&Self::path(key))
            .await
            .map(|m| m.size)
            .map_err(Self::map_err)
    }

    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        let r = self
            .inner
            .put(&Self::path(key), body.into())
            .await
            .map_err(Self::map_err)?;
        // ⚠️ A backend may return no ETag. Empty is not a usable CAS token, so a later
        // conditional write on it must fail rather than appear to work.
        Ok(PutOutcome {
            tag: CasTag::new(r.e_tag.unwrap_or_default()),
        })
    }

    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        match self
            .inner
            .put_opts(&Self::path(key), body.into(), Self::put_opts(&pre))
            .await
        {
            Ok(r) => Ok(PutOutcome {
                tag: CasTag::new(r.e_tag.unwrap_or_default()),
            }),
            Err(e) => Err(map_cas_err(&e)),
        }
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        // One request per key. ⚠️ S3 offers DeleteObjects for 1000 at a time and this
        // does not use it, so GC costs 1000x what it should. Recorded rather than fixed:
        // it needs `delete_stream`, which needs the futures machinery, and M0a's job here
        // is to learn whether the trait fits — not to optimise an unverified adapter.
        for key in keys {
            self.inner
                .delete(&Self::path(key))
                .await
                .map_err(Self::map_err)?;
        }
        Ok(())
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        // `list_with_delimiter` rather than the streaming `list`, so the adapter needs no
        // futures dependency. Listing is banned on every hot path anyway, so the
        // non-streaming form is the right shape for the three sanctioned uses.
        let r = self
            .inner
            .list_with_delimiter(Some(&Self::path(prefix)))
            .await
            .map_err(Self::map_err)?;
        Ok(r.objects
            .into_iter()
            .map(|m| Key::new(m.location.to_string()))
            .collect())
    }
}

/// Maps an `object_store` error onto the 412/409 split the retry policy rests on.
///
/// Free-standing so it can be tested directly: the 409 path is reachable only from a real
/// S3 under concurrent conditional writes, which is precisely what we cannot produce.
///
/// ⚠️ **A finding for M0b.** `object_store` has typed variants for 412
/// (`AlreadyExists`, `Precondition`) but folds S3's `409 ConditionalRequestConflict` into
/// a generic error, so it is recovered by matching the message. A string match on a
/// third-party error is fragile, and confirming it against a real throttle is an M0b
/// exit item rather than something to assume.
pub(crate) fn map_cas_err(e: &object_store::Error) -> CasError {
    match e {
        object_store::Error::AlreadyExists { .. } | object_store::Error::Precondition { .. } => {
            CasError::Lost
        }
        other => {
            let s = other.to_string();
            if s.contains("409") || s.contains("ConditionalRequestConflict") {
                CasError::Contended
            } else {
                CasError::Io(s)
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn generic(msg: &str) -> object_store::Error {
        object_store::Error::Generic {
            store: "test",
            source: msg.to_owned().into(),
        }
    }

    #[test]
    fn a_412_is_lost_so_the_caller_rebases() {
        assert_eq!(
            map_cas_err(&object_store::Error::Precondition {
                path: "k".to_owned(),
                source: "412".to_owned().into()
            }),
            CasError::Lost
        );
        assert_eq!(
            map_cas_err(&object_store::Error::AlreadyExists {
                path: "k".to_owned(),
                source: "exists".to_owned().into()
            }),
            CasError::Lost
        );
    }

    #[test]
    fn a_409_is_contended_so_the_caller_retries_without_rebasing() {
        // Conflating this with Lost is what produces a rebase storm: every transient
        // conflict would rebuild the world.
        assert_eq!(
            map_cas_err(&generic("ConditionalRequestConflict")),
            CasError::Contended
        );
        assert_eq!(
            map_cas_err(&generic("status 409 from S3")),
            CasError::Contended
        );
    }

    #[test]
    fn anything_else_is_neither_and_must_not_be_guessed_at() {
        assert!(matches!(
            map_cas_err(&generic("connection reset")),
            CasError::Io(_)
        ));
    }

    #[test]
    fn a_slowdown_is_recognised_wherever_it_appears() {
        assert_eq!(
            ObjectStoreBackend::map_err(generic("SlowDown")),
            BlobError::SlowDown
        );
        assert_eq!(
            ObjectStoreBackend::map_err(generic("503 from origin")),
            BlobError::SlowDown
        );
        assert!(matches!(
            ObjectStoreBackend::map_err(generic("boom")),
            BlobError::Other(_)
        ));
        assert!(matches!(
            ObjectStoreBackend::map_err(object_store::Error::NotFound {
                path: "k".to_owned(),
                source: "gone".to_owned().into()
            }),
            BlobError::NotFound(_)
        ));
    }
}
