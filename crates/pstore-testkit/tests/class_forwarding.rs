//! Every decorator forwards the cache [`Class`], on every read that carries one.
//!
//! ⚠️ **The hazard is written down in the trait and tested nowhere.** `BlobStore`'s own doc on
//! `get_range_as` says it: *"Defaulted, and every decorator must forward it. A decorator that
//! inherits this default drops the class on the floor, and the read is then admitted as
//! `Bulk` — D-21 disabled, with every test still passing."*
//!
//! That last clause is the whole problem. `pstore-cache`'s admission tests pass a class in at
//! the top of a one-layer stack and see it arrive; nothing checks what happens when a
//! decorator sits between them. A `Congested` or `Faulty` that inherited the default would
//! silently turn every classed read into `Bulk`, centroid tables would become evictable, and
//! the only symptom would be a cache hit rate nobody is watching.
//!
//! Covered here: the decorators that can wrap an arbitrary backend, which is the same thing as
//! the decorators that can appear in a serving stack.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "assertions in tests are the reporting mechanism"
)]

use bytes::Bytes;
use pstore_blob::{
    Accounted, BlobError, BlobStore, Capabilities, CasError, Class, Congested, Faults, Faulty, Key,
    MemoryStore, Precondition, PutOutcome,
};
use pstore_types::{CasTag, TenantId};
use std::sync::{Arc, Mutex};

/// A correct backend that records the class of every classed read it is asked for.
/// ⚠️ `Clone` with **shared** state, because every decorator takes its backend by value and
/// wraps it in an `Arc` of its own — so the test cannot hold the same `Arc` the decorator
/// does, and has to hold a clone that sees the same recordings.
#[derive(Debug, Default, Clone)]
struct Recorder {
    inner: MemoryStore,
    seen: Arc<Mutex<Vec<(&'static str, Class)>>>,
}

impl Recorder {
    fn note(&self, method: &'static str, class: Class) {
        if let Ok(mut s) = self.seen.lock() {
            s.push((method, class));
        }
    }
    fn seen(&self) -> Vec<(&'static str, Class)> {
        self.seen
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }
}

#[async_trait::async_trait]
impl BlobStore for Recorder {
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
    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: Class,
    ) -> Result<Bytes, BlobError> {
        self.note("get_range_as", class);
        self.inner.get_range(key, range).await
    }
    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        self.note("get_suffix_as", class);
        self.inner.get_suffix(key, n).await
    }
    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        self.note("get_immutable", class);
        self.inner.get(key).await
    }
}

const KEY: &str = "seg/0001";

async fn seeded() -> Recorder {
    let r = Recorder::default();
    r.put(&Key::new(KEY), Bytes::from_static(&[7u8; 512]))
        .await
        .unwrap();
    if let Ok(mut s) = r.seen.lock() {
        s.clear();
    }
    r
}

/// Drives the three classed reads through `s`, each with a class that is **not** the default.
///
/// ⚠️ Not `Bulk`. `Class` derives `Default` as `Bulk`, so a decorator that drops the class
/// entirely still produces `Bulk` at the bottom — and a fixture that asked for `Bulk` would
/// pass against exactly the bug it exists to catch.
async fn drive<S: BlobStore>(s: &S) {
    let k = Key::new(KEY);
    s.get_range_as(&k, 0..16, Class::Pinned).await.unwrap();
    s.get_suffix_as(&k, 16, Class::Meta).await.unwrap();
    s.get_immutable(&k, Class::Pinned).await.unwrap();
}

fn expected() -> Vec<(&'static str, Class)> {
    vec![
        ("get_range_as", Class::Pinned),
        ("get_suffix_as", Class::Meta),
        ("get_immutable", Class::Pinned),
    ]
}

#[tokio::test]
async fn congested_forwards_the_class() {
    let r = seeded().await;
    let s = Congested::new(r.clone(), 8);
    drive(&s).await;
    assert_eq!(r.seen(), expected());
}

#[tokio::test]
async fn faulty_forwards_the_class() {
    let r = seeded().await;
    let s = Faulty::new(r.clone(), 1, Faults::none());
    drive(&s).await;
    assert_eq!(r.seen(), expected());
}

#[tokio::test]
async fn accounting_forwards_the_class() {
    let r = seeded().await;
    let acc = Accounted::new(r.clone());
    drive(&acc.as_tenant(TenantId(1))).await;
    assert_eq!(r.seen(), expected());
}

#[tokio::test]
async fn depth_counting_forwards_the_class() {
    let r = seeded().await;
    let s = pstore_testkit::depth::DepthCounting::new(r.clone());
    drive(&s).await;
    assert_eq!(r.seen(), expected());
}

#[tokio::test]
async fn auditing_forwards_the_class() {
    let r = seeded().await;
    let s = pstore_testkit::audit::Auditing::new(r.clone());
    drive(&s).await;
    assert_eq!(r.seen(), expected());
}

#[tokio::test]
async fn a_whole_stack_forwards_the_class() {
    // ⚠️ The case a single-decorator test cannot reach: each layer forwarding is necessary
    // and not sufficient, because `get_ranges_as` dispatches back through `self.get_range_as`
    // and one inherited default anywhere in the stack erases the class for everything above
    // it too.
    let r = seeded().await;
    let stack = Congested::new(
        Faulty::new(
            pstore_testkit::depth::DepthCounting::new(r.clone()),
            3,
            Faults::none(),
        ),
        8,
    );
    drive(&stack).await;
    assert_eq!(r.seen(), expected());

    // And through the batched path, which is the one every real query uses.
    if let Ok(mut s) = r.seen.lock() {
        s.clear();
    }
    stack
        .get_ranges_as(&Key::new(KEY), &[0..8, 400..416], Class::Meta)
        .await
        .unwrap();
    let seen = r.seen();
    assert!(!seen.is_empty(), "the batched read reached nothing");
    assert!(
        seen.iter()
            .all(|(m, c)| *m == "get_range_as" && *c == Class::Meta),
        "the batched path lost the class: {seen:?}"
    );
}
