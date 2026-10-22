//! M121: a pin check's length and tag are one probe.
//!
//! ⚠️ Counted at the boundary, as `batch_delete.rs` counts deletes: `head` and `get_tag` are
//! each correct, so only the requests they send can tell one HEAD from two.

#![cfg(feature = "object_store")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertions in tests are the reporting mechanism"
)]

use futures_util::stream::BoxStream;
use object_store::{ObjectStore, path::Path};
use pstore_blob::{BlobStore, Key, ObjectStoreBackend};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A correct `ObjectStore` that counts the HEADs it is sent. `ObjectStoreExt::head` is
/// `get_opts` with `head` set, in `object_store` 0.14.1, so that is where one is seen.
#[derive(Debug, Default)]
struct Heads {
    inner: object_store::memory::InMemory,
    heads: AtomicUsize,
}

impl std::fmt::Display for Heads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("heads")
    }
}

#[async_trait::async_trait]
impl ObjectStore for Heads {
    async fn put_opts(
        &self,
        location: &Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }
    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }
    async fn get_opts(
        &self,
        location: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if options.head {
            self.heads.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.get_opts(location, options).await
    }
    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }
    fn list(
        &self,
        prefix: Option<&Path>,
    ) -> BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(
        &self,
        prefix: Option<&Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[tokio::test]
async fn a_stat_is_one_head() {
    let heads = Arc::new(Heads::default());
    let s = ObjectStoreBackend::new(
        Arc::clone(&heads) as Arc<dyn ObjectStore>,
        ObjectStoreBackend::unprobed("heads"),
    );
    let k = Key::new("a/b");
    s.put(&k, bytes::Bytes::from_static(b"0123456789"))
        .await
        .unwrap();
    heads.heads.store(0, Ordering::Relaxed);

    let (len, tag) = s.stat(&k).await.unwrap();
    assert_eq!(
        heads.heads.load(Ordering::Relaxed),
        1,
        "one probe, both answers"
    );
    // The answers are the two calls' own.
    assert_eq!(len, s.head(&k).await.unwrap());
    assert_eq!(tag, s.get_tag(&k).await.unwrap());
    assert!(tag.is_some());
    // A missing object is the length's refusal, as `head` reports it.
    let gone = Key::new("a/none");
    let (want, got) = (
        s.head(&gone).await.unwrap_err(),
        s.stat(&gone).await.unwrap_err(),
    );
    assert_eq!(got.to_string(), want.to_string());
}
