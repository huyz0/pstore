//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `Arc<dyn BlobStore>` is a store (M22): every method reaches the store inside, the defaulted
//! ones with their class and their own plan, not the trait's default re-derivation.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_types::CasTag;
use std::ops::Range;
use std::sync::{Arc, Mutex};

/// Records which method was called, and with what class.
#[derive(Debug, Default)]
struct Recorder {
    inner: MemoryStore,
    calls: Mutex<Vec<String>>,
}

impl Recorder {
    fn saw(&self, what: impl Into<String>) {
        self.calls.lock().unwrap().push(what.into());
    }
}

#[async_trait::async_trait]
impl BlobStore for Recorder {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.saw("get");
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.saw("get_range");
        self.inner.get_range(key, range).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        class: Class,
    ) -> Result<Bytes, BlobError> {
        self.saw(format!("get_range_as {class:?}"));
        self.inner.get_range(key, range).await
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[Range<u64>],
        class: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        self.saw(format!("get_ranges_as {class:?}"));
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        self.saw(format!("get_immutable {class:?}"));
        self.inner.get(key).await
    }
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        self.saw("get_ranges");
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.saw("get_suffix");
        self.inner.get_suffix(key, n).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        self.saw(format!("get_suffix_as {class:?}"));
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.saw("get_with_tag");
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.saw("get_tag");
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.saw("head");
        self.inner.head(key).await
    }
    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.saw("put");
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        self.saw("put_conditional");
        self.inner.put_conditional(key, body, pre).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.saw("delete_batch");
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.saw("list_unrestricted");
        self.inner.list_unrestricted(prefix).await
    }
}

#[tokio::test]
async fn every_method_reaches_the_store_inside_with_its_class() {
    let rec = Arc::new(Recorder::default());
    let s: Arc<dyn BlobStore> = Arc::clone(&rec) as Arc<dyn BlobStore>;
    let k = Key::new("k");
    s.put(&k, Bytes::from_static(b"abcdefgh")).await.unwrap();
    s.get(&k).await.unwrap();
    s.get_range(&k, 0..2).await.unwrap();
    s.get_range_as(&k, 0..2, Class::Meta).await.unwrap();
    s.get_ranges_as(&k, &[0..1, 4..5], Class::Scan)
        .await
        .unwrap();
    s.get_immutable(&k, Class::Pinned).await.unwrap();
    s.get_ranges(&k, &[0..1, 4..5]).await.unwrap();
    s.get_suffix(&k, 2).await.unwrap();
    s.get_suffix_as(&k, 2, Class::Bulk).await.unwrap();
    let (_, tag) = s.get_with_tag(&k).await.unwrap();
    s.get_tag(&k).await.unwrap();
    s.head(&k).await.unwrap();
    s.put_conditional(&k, Bytes::from_static(b"x"), Precondition::Match(tag))
        .await
        .unwrap();
    s.list_unrestricted(&Key::new("")).await.unwrap();
    s.delete_batch(&[k]).await.unwrap();
    assert_eq!(s.capabilities(), rec.capabilities());
    assert_eq!(
        *rec.calls.lock().unwrap(),
        [
            "put",
            "get",
            "get_range",
            "get_range_as Meta",
            "get_ranges_as Scan",
            "get_immutable Pinned",
            "get_ranges",
            "get_suffix",
            "get_suffix_as Bulk",
            "get_with_tag",
            "get_tag",
            "head",
            "put_conditional",
            "list_unrestricted",
            "delete_batch",
        ]
    );
}
