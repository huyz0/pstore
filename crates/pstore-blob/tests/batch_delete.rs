//! Criterion 10: GC's request rate must not scale with objects.
//!
//! ⚠️ Counted at the boundary rather than read from the code. The loop this replaces was
//! *correct* — it deleted every key — so no functional assertion anywhere could tell the two
//! apart, and M0a recorded the 1000× cost and shipped it.

#![cfg(feature = "object_store")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertions in tests are the reporting mechanism"
)]

use futures_util::stream::BoxStream;
use futures_util::{StreamExt, TryStreamExt};
use object_store::{ObjectStore, ObjectStoreExt, path::Path};
use pstore_blob::{BlobStore, Key, MemoryStore, ObjectStoreBackend};
use std::sync::{Arc, Mutex};

/// A correct `ObjectStore` that records how it was asked to delete.
///
/// ⚠️ **`delete_stream` is the only thing worth counting**, and that is a fact about
/// `object_store` 0.14.1 rather than a convenience: `ObjectStoreExt::delete` is itself
/// implemented as `delete_stream` over a one-element stream (`lib.rs:1530`). So the adapter's
/// old per-key loop was 250 calls carrying one path each, and the new code is one call
/// carrying 250 — which is exactly the difference, and the batch sizes are what record it.
#[derive(Debug, Default)]
struct Counting {
    inner: Arc<object_store::memory::InMemory>,
    batches: Arc<Mutex<Vec<usize>>>,
}

impl Counting {
    fn batches(&self) -> Vec<usize> {
        self.batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl std::fmt::Display for Counting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("counting")
    }
}

#[async_trait::async_trait]
impl ObjectStore for Counting {
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
        self.inner.get_opts(location, options).await
    }
    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        let inner = Arc::clone(&self.inner);
        let batches = Arc::clone(&self.batches);
        futures_util::stream::once(async move {
            let paths: Vec<Path> = locations.try_collect().await?;
            if let Ok(mut b) = batches.lock() {
                b.push(paths.len());
            }
            for p in &paths {
                inner.delete(p).await?;
            }
            Ok::<Vec<Path>, object_store::Error>(paths)
        })
        .flat_map(|r| match r {
            Ok(paths) => futures_util::stream::iter(paths.into_iter().map(Ok).collect::<Vec<_>>()),
            Err(e) => futures_util::stream::iter(vec![Err(e)]),
        })
        .boxed()
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

async fn seeded(n: usize) -> (Arc<Counting>, ObjectStoreBackend, Vec<Key>) {
    let counting = Arc::new(Counting::default());
    let s = ObjectStoreBackend::new(
        Arc::clone(&counting) as Arc<dyn ObjectStore>,
        ObjectStoreBackend::unprobed("counting"),
    );
    let keys: Vec<Key> = (0..n).map(|i| Key::new(format!("k/{i:05}"))).collect();
    for k in &keys {
        s.put(k, bytes::Bytes::from_static(b"x")).await.unwrap();
    }
    if let Ok(mut b) = counting.batches.lock() {
        b.clear();
    }
    (counting, s, keys)
}

#[tokio::test]
async fn a_batch_delete_is_one_call_into_the_backend() {
    let (counting, s, keys) = seeded(250).await;
    s.delete_batch(&keys).await.unwrap();

    // One call carrying 250 paths. The loop this replaces was 250 calls carrying one.
    assert_eq!(counting.batches(), vec![250]);
    // And it actually deleted. ⚠️ `delete_stream` is lazy: a stream that is never polled
    // costs nothing and reaps nothing, which is *worse* than the loop it replaced, because
    // GC then prunes the graveyard entry that was the only record these objects exist.
    for k in &keys {
        assert!(s.get(k).await.is_err(), "{k} survived the batch delete");
    }
}

#[tokio::test]
async fn an_over_cap_batch_is_refused_by_both_implementations() {
    // ⚠️ The same contract on both, deliberately. `Engine::gc` builds an unbounded batch, so
    // an adapter that chunked where `MemoryStore` refuses would make GC's behaviour above the
    // cap depend on which backend is underneath — and every engine test runs on
    // `MemoryStore`, so nothing would ever see it.
    let (counting, s, _) = seeded(0).await;
    let cap = s.capabilities().max_batch_delete;
    let too_many: Vec<Key> = (0..=cap).map(|i| Key::new(format!("k/{i:05}"))).collect();

    let adapter = s.delete_batch(&too_many).await.expect_err("must refuse");
    let memory = MemoryStore::new()
        .delete_batch(&too_many)
        .await
        .expect_err("must refuse");
    assert_eq!(adapter.to_string(), memory.to_string());
    assert!(
        counting.batches().is_empty(),
        "a refused batch still deleted"
    );
}

#[tokio::test]
async fn an_empty_batch_costs_nothing() {
    let (counting, s, _) = seeded(0).await;
    s.delete_batch(&[]).await.unwrap();
    assert!(counting.batches().is_empty());
}
