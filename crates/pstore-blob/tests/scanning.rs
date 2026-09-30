//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! `Scanning` (M20): every bulk or unclassed read reaches the store as `Class::Scan`, and
//! every other class arrives unchanged (D-50).

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Class, Key, MemoryStore, Precondition,
    PutOutcome, Scanning,
};
use pstore_types::CasTag;
use std::ops::Range;
use std::sync::{Arc, Mutex};

/// A store that records the class each classed read arrived with, and `None` for a read
/// that arrived with no class at all.
#[derive(Default)]
struct Classes {
    inner: MemoryStore,
    seen: Mutex<Vec<Option<Class>>>,
}

impl Classes {
    fn saw(&self, c: Option<Class>) {
        self.seen.lock().unwrap().push(c);
    }
}

#[async_trait::async_trait]
impl BlobStore for Classes {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.saw(None);
        self.inner.get_range(key, range).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        class: Class,
    ) -> Result<Bytes, BlobError> {
        self.saw(Some(class));
        self.inner.get_range(key, range).await
    }
    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        self.saw(None);
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[Range<u64>],
        class: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        self.saw(Some(class));
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.saw(None);
        self.inner.get_suffix(key, n).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        self.saw(Some(class));
        self.inner.get_suffix(key, n).await
    }
    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        self.saw(Some(class));
        self.inner.get(key).await
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
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.inner.get_tag(key).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

#[tokio::test]
async fn bulk_and_unclassed_reads_arrive_as_scans_and_others_unchanged() {
    let inner = Arc::new(Classes::default());
    let k = Key::new("seg");
    inner.put(&k, Bytes::from(vec![7u8; 64])).await.unwrap();
    let s = Scanning::new(Arc::clone(&inner));
    s.get_range(&k, 0..8).await.unwrap();
    s.get_ranges(&k, &[0..8, 16..24]).await.unwrap();
    s.get_suffix(&k, 8).await.unwrap();
    for c in [Class::Bulk, Class::Meta, Class::Pinned] {
        s.get_range_as(&k, 0..8, c).await.unwrap();
        s.get_ranges_as(&k, &[0..8, 16..24], c).await.unwrap();
        s.get_suffix_as(&k, 8, c).await.unwrap();
        s.get_immutable(&k, c).await.unwrap();
    }
    let scan = Some(Class::Scan);
    let (meta, pinned) = (Some(Class::Meta), Some(Class::Pinned));
    assert_eq!(
        *inner.seen.lock().unwrap(),
        [
            scan, scan, scan, // unclassed
            scan, scan, scan, scan, // bulk
            meta, meta, meta, meta, // meta
            pinned, pinned, pinned, pinned, // pinned
        ]
    );
    // And a `get`, which names no class and is never cached, passes through.
    assert_eq!(s.get(&k).await.unwrap().len(), 64);
}
