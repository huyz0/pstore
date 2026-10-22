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
use futures_util::StreamExt;
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
    /// GCS's batch endpoint (M107), which `delete_batch` uses in place of `delete_stream`.
    #[cfg(feature = "gcs")]
    batch: Option<Arc<crate::gcs_batch::GcsBatch>>,
}

impl ObjectStoreBackend {
    /// Wraps `inner` with a capability profile that **came from the conformance suite**.
    ///
    /// Not from what the backend claims: MinIO advertises conditional writes and then
    /// ignores the `If-None-Match: *` wildcard, so a hand-set profile is a statement of
    /// hope. See [`Self::unprobed`] for the pre-measurement value.
    #[must_use]
    pub fn new(inner: Arc<dyn ObjectStore>, caps: Capabilities) -> Self {
        Self {
            inner,
            caps,
            #[cfg(feature = "gcs")]
            batch: None,
        }
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
            // Every backend but Azure serves it; `azure` says otherwise.
            suffix_read: true,
        }
    }

    /// Wraps an Azure store (M46): `caps` with what the Azure client does, whatever profile
    /// it came from.
    /// - No suffix reads: the client refuses `GetRange::Suffix` (C-14), so a segment open
    ///   that knows no length asks `head` first, billed, above this adapter.
    /// - Batches of 256: the client splits a delete into requests of 256 keys, and one
    ///   `delete_batch` must be one request, or `Accounted` bills one for several.
    /// - Deletes are billed.
    #[must_use]
    pub fn azure(inner: Arc<dyn ObjectStore>, caps: Capabilities) -> Self {
        Self::new(
            inner,
            Capabilities {
                suffix_read: false,
                max_batch_delete: 256,
                delete_is_free: false,
                ..caps
            },
        )
    }

    /// Wraps a GCS store (M107): `caps` with what GCS's batch endpoint does, whatever profile
    /// it came from.
    /// - Batches of 100, the API's cap, sent as **one** `POST {endpoint}/batch/storage/v1`
    ///   through `client`, with the store's own credentials. `object_store` sends one
    ///   `DELETE` per object, and `Accounted` bills one request for the lot.
    /// - Deletes are billed.
    ///
    /// `endpoint` is the JSON API's root, `https://storage.googleapis.com` on GCS, since the
    /// store does not say where it points.
    #[cfg(feature = "gcs")]
    #[must_use]
    pub fn gcs(
        store: object_store::gcp::GoogleCloudStorage,
        client: object_store::client::HttpClient,
        endpoint: &str,
        bucket: &str,
        caps: Capabilities,
    ) -> Self {
        let batch = crate::gcs_batch::GcsBatch {
            client,
            credentials: Arc::clone(store.credentials()),
            endpoint: endpoint.to_owned(),
            bucket: bucket.to_owned(),
        };
        Self {
            inner: Arc::new(store),
            caps: Capabilities {
                max_batch_delete: crate::gcs_batch::MAX_BATCH,
                delete_is_free: false,
                ..caps
            },
            batch: Some(Arc::new(batch)),
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

    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        // ⚠️ `.ok()` was here, and it is the defect at its source: a 503 and a missing
        // object produced the same `None`. Only NotFound is absence; everything else is a
        // failed probe, and an object with no ETag is a backend that cannot fence — also
        // absence of a tag, but not absence of the object, so it stays `Ok(None)` and the
        // CAS that follows is refused by the capability guard rather than here.
        match self.inner.head(&Self::path(key)).await {
            Ok(m) => Ok(m.e_tag.map(CasTag::new)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(Self::map_err(e)),
        }
    }

    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.inner
            .head(&Self::path(key))
            .await
            .map(|m| m.size)
            .map_err(Self::map_err)
    }

    async fn stat(&self, key: &Key) -> Result<(u64, Option<CasTag>), BlobError> {
        // One HEAD carries both (M121): `head` and `get_tag` were a billed request each.
        // Absence is `head`'s refusal, which the default judges first too.
        self.inner
            .head(&Self::path(key))
            .await
            .map(|m| (m.size, m.e_tag.map(CasTag::new)))
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
        // ⚠️ **One call, not one per key.** The loop this replaces made GC's request rate
        // scale with *objects*, which is an AGENTS.md "Never" — S3 deletes 1000 at a time
        // and `delete_stream` is where `object_store` exposes that. M0a recorded the cost
        // (1000x) and left it; M0a.12, closed here.
        //
        // ⚠️ **Over-cap is refused, not chunked**, because `MemoryStore` already refuses it
        // and `BlobStore::delete_batch` is documented "capped at `max_batch_delete`".
        // Chunking here would make one trait method error on one implementation and succeed
        // on the other, and `Engine::gc` builds an unbounded batch — so the difference would
        // surface as a backend-dependent GC, invisible to every test, since they all run on
        // `MemoryStore`.
        if keys.len() > self.caps.max_batch_delete {
            return Err(BlobError::Other(format!(
                "batch of {} exceeds the backend's limit of {}",
                keys.len(),
                self.caps.max_batch_delete
            )));
        }
        if keys.is_empty() {
            return Ok(());
        }
        #[cfg(feature = "gcs")]
        if let Some(batch) = &self.batch {
            return batch.delete(keys).await;
        }
        let paths: Vec<Result<Path, object_store::Error>> =
            keys.iter().map(|k| Ok(Self::path(k))).collect();
        let stream = futures_util::stream::iter(paths).boxed();
        // ⚠️ Drained, not dropped. `delete_stream` is lazy: a stream that is never polled
        // deletes nothing and returns no error, so GC would report success having reaped
        // nothing — the graveyard entry is pruned and the objects leak with nothing left to
        // name them by.
        let mut out = self.inner.delete_stream(stream);
        while let Some(r) = out.next().await {
            r.map_err(Self::map_err)?;
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
    clippy::indexing_slicing,
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
    fn azure_declares_no_suffix_and_batches_of_256() {
        // M46: what the Azure client does, whatever profile the caps came from.
        // `delete_is_free: true` in, so the override is visible (the sweep found it was not).
        let profile = Capabilities {
            cas: Support::Supported,
            create_if_absent: Support::Supported,
            delete_is_free: true,
            ..ObjectStoreBackend::unprobed("azure")
        };
        let az = ObjectStoreBackend::azure(
            Arc::new(object_store::memory::InMemory::new()),
            profile.clone(),
        );
        let caps = crate::BlobStore::capabilities(&az);
        assert!(!caps.suffix_read);
        assert_eq!(caps.max_batch_delete, 256);
        assert!(!caps.delete_is_free);
        // The profile's own claims survive.
        assert_eq!(caps.cas, Support::Supported);
        assert_eq!(caps.create_if_absent, Support::Supported);
        // Every other backend serves suffix reads.
        assert!(profile.suffix_read);
        assert!(crate::BlobStore::capabilities(&crate::MemoryStore::new()).suffix_read);
    }

    /// A GCS store that never leaves the process: building one makes no request.
    #[cfg(feature = "gcs")]
    fn gcs_over(gcs: &crate::gcs_batch::mock::Gcs, caps: Capabilities) -> ObjectStoreBackend {
        let store = object_store::gcp::GoogleCloudStorageBuilder::new()
            .with_bucket_name("pstore")
            .with_service_account_key(
                r#"{"gcs_base_url": "http://gcs.invalid:1", "disable_oauth": true,
                    "client_email": "", "private_key_id": "", "private_key": ""}"#,
            )
            .build()
            .unwrap();
        let client = object_store::client::HttpClient::new(gcs.clone());
        ObjectStoreBackend::gcs(store, client, "http://gcs.invalid:1", "pstore", caps)
    }

    #[cfg(feature = "gcs")]
    #[tokio::test]
    async fn gcs_declares_batches_of_100_and_sends_each_as_one_request() {
        use crate::BlobStore;
        // `delete_is_free: true` and 1000 in, so both overrides are visible.
        let profile = Capabilities {
            cas: Support::Supported,
            delete_is_free: true,
            ..ObjectStoreBackend::unprobed("gcs")
        };
        let body = "--b\r\nContent-Type: application/http\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n\r\n\
                    --b\r\nContent-Type: application/http\r\n\r\nHTTP/1.1 404 Not Found\r\n\r\n\r\n--b--";
        let gcs = crate::gcs_batch::mock::Gcs::answering(200, "multipart/mixed; boundary=b", body);
        let store = gcs_over(&gcs, profile);
        let caps = store.capabilities();
        assert_eq!(caps.max_batch_delete, 100);
        assert!(!caps.delete_is_free);
        assert_eq!(caps.cas, Support::Supported);

        store.delete_batch(&[]).await.unwrap();
        assert_eq!(gcs.sent().len(), 0, "an empty batch sent a request");
        let over: Vec<Key> = (0..101).map(|i| Key::new(format!("k{i}"))).collect();
        assert!(store.delete_batch(&over).await.is_err());
        assert_eq!(gcs.sent().len(), 0, "an over-cap batch sent a request");

        store
            .delete_batch(&[Key::new("a/b"), Key::new("c")])
            .await
            .unwrap();
        let sent = gcs.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, "http://gcs.invalid:1/batch/storage/v1");
        assert!(sent[0].4.contains("/o/a%2Fb HTTP/1.1"), "{}", sent[0].4);
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
