//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The read cache.
//!
//! ⚠️ **Every request count here fixes `coalesce_gap = 256` and spaces its ranges wider than
//! that.** `MemoryStore` defaults to 64 KiB, at which every range in a small fixture merges
//! into a single fetch — and a cache keyed on the coalesced span then scores exactly what a
//! correct one scores. The gap is what makes these numbers discriminate at all.

use bytes::Bytes;
use pstore_blob::{Accounted, BlobStore, Key, MemoryStore, OpClass, Precondition, TenantView};
use pstore_cache::Caching;
use pstore_types::TenantId;
use std::ops::Range;
use std::sync::Arc;

const GAP: u64 = 256;
const BODY: usize = 8192;

fn body() -> Bytes {
    Bytes::from((0..BODY).map(|i| (i % 251) as u8).collect::<Vec<u8>>())
}

/// `Caching` OUTERMOST, accounting beneath — the only order in which a hit can be observed to
/// cost nothing, and the only one in which the cache sees requested rather than coalesced
/// ranges. ⚠️ Reversed, every one of these tests still compiles and several still pass.
async fn fixture(
    budget: usize,
) -> (
    Caching<TenantView<MemoryStore>>,
    Arc<Accounted<MemoryStore>>,
    Key,
) {
    let acct = Arc::new(Accounted::new(MemoryStore::with_coalesce_gap(GAP)));
    let key = Key::new("seg/one");
    let view = acct.as_tenant(TenantId(1));
    view.put(&key, body()).await.unwrap();
    // ⚠️ One arena, holding exactly `budget`. `Caching::new` splits the budget across classes,
    // so these tests — which are about the LRU and the budget bound, not about classes —
    // would otherwise be measuring a bulk quota of 80% of what they asked for.
    let cache = Caching::with_quotas(Arc::new(acct.as_tenant(TenantId(1))), 0, 0, budget);
    (cache, acct, key)
}

fn reads(acct: &Accounted<MemoryStore>) -> u64 {
    acct.count(TenantId(1), OpClass::Read)
}

/// Ranges spaced far enough apart that coalescing will not merge them.
fn spread(n: usize) -> Vec<Range<u64>> {
    (0..n)
        .map(|i| {
            let at = i as u64 * (GAP * 4);
            at..at + 64
        })
        .collect()
}

#[tokio::test]
async fn a_second_read_of_the_same_ranges_costs_nothing() {
    let (cache, acct, key) = fixture(1 << 20).await;
    let want = spread(3);

    let first = cache.get_ranges(&key, &want).await.unwrap();
    let after_first = reads(&acct);
    assert!(
        after_first > 0,
        "the first read reached the store not at all"
    );

    let second = cache.get_ranges(&key, &want).await.unwrap();
    assert_eq!(
        reads(&acct),
        after_first,
        "a repeated read reached the store again"
    );
    assert_eq!(first, second, "the cached answer differed from the first");

    // The same for the other two intercepted methods.
    let r = 100..300;
    cache.get_range(&key, r.clone()).await.unwrap();
    let n = reads(&acct);
    cache.get_range(&key, r).await.unwrap();
    assert_eq!(reads(&acct), n, "a repeated get_range reached the store");

    cache.get_suffix(&key, 128).await.unwrap();
    let n = reads(&acct);
    cache.get_suffix(&key, 128).await.unwrap();
    assert_eq!(reads(&acct), n, "a repeated get_suffix reached the store");
}

#[tokio::test]
async fn a_cached_range_equals_what_the_store_holds() {
    // ⚠️ A cache that is fast and wrong passes the criterion above. This is the one that
    // catches an off-by-one in the key: identical, overlapping, adjacent and nested.
    let (cache, _, key) = fixture(1 << 20).await;
    let truth = MemoryStore::with_coalesce_gap(GAP);
    truth.put(&key, body()).await.unwrap();

    let cases: Vec<Range<u64>> = vec![
        0..64,
        0..64,
        32..96,
        64..128,
        0..1,
        10..20,
        0..4096,
        4095..4096,
        1000..2000,
        1500..1600,
    ];
    for r in cases {
        let got = cache.get_range(&key, r.clone()).await.unwrap();
        let want = truth.get_range(&key, r.clone()).await.unwrap();
        assert_eq!(got, want, "cached bytes differ from the store for {r:?}");
    }

    // And through the batched path, where the ranges are re-assembled from fetched spans.
    let rs = vec![0..64, 32..96, 4000..4096];
    let got = cache.get_ranges(&key, &rs).await.unwrap();
    let want = truth.get_ranges(&key, &rs).await.unwrap();
    assert_eq!(got, want, "batched cached bytes differ from the store");
}

#[tokio::test]
async fn overlapping_probe_sets_share_their_overlap() {
    // ⚠️ The criterion that pins the KEY. A cache keyed on the coalesced span scores 3 here
    // and 0 on the first test; a cache keyed on the requested range scores 2 and 0. Only the
    // pair distinguishes them, which is why both exist.
    let (cache, acct, key) = fixture(1 << 20).await;
    let all = spread(3);

    cache.get_ranges(&key, &all[..1]).await.unwrap();
    let after_first = reads(&acct);

    cache.get_ranges(&key, &all).await.unwrap();
    assert_eq!(
        reads(&acct) - after_first,
        2,
        "a second call sharing one of three ranges should fetch the other two"
    );
}

#[tokio::test]
async fn a_mutable_object_is_never_served_stale() {
    // ⚠️ The landmine. Segment immutability is what makes a cache entry correct forever, and
    // it does not extend to whole-object `get`: the lane registry is read that way and
    // CAS-mutated. A cached registry makes a newly registered lane permanently invisible and
    // its bundles unrecoverable — silent data loss, from a cache passing every hit-rate test.
    let (cache, _, key) = fixture(1 << 20).await;
    let before = cache.get(&key).await.unwrap();

    // Change the object underneath the cache, through the very store it wraps.
    cache
        .inner()
        .put(&key, Bytes::from_static(b"rewritten"))
        .await
        .unwrap();

    let after = cache.get(&key).await.unwrap();
    assert_ne!(
        before, after,
        "a `get` was served from cache after the object changed"
    );
    assert_eq!(after, Bytes::from_static(b"rewritten"));
}

#[tokio::test]
async fn the_cache_stays_within_its_budget() {
    // A budget that is checked but never enforced passes every test above.
    let budget = 4096;
    let (cache, _, key) = fixture(budget).await;
    for i in 0..64u64 {
        let at = i * 128;
        cache.get_range(&key, at..at + 128).await.unwrap();
        assert!(
            cache.resident_bytes() <= budget,
            "resident {} bytes over a budget of {budget}",
            cache.resident_bytes()
        );
    }
    assert!(cache.resident_bytes() > 0, "the cache evicted everything");
}

#[tokio::test]
async fn an_evicted_range_is_re_read_correctly() {
    // Eviction that drops the bytes but keeps the key returns an empty answer that looks
    // like a hit.
    let (cache, _, key) = fixture(2048).await;
    let truth = MemoryStore::with_coalesce_gap(GAP);
    truth.put(&key, body()).await.unwrap();

    let ranges: Vec<Range<u64>> = (0..40u64).map(|i| i * 128..i * 128 + 128).collect();
    for r in &ranges {
        cache.get_range(&key, r.clone()).await.unwrap();
    }
    // Everything, including whatever was evicted along the way.
    for r in &ranges {
        let got = cache.get_range(&key, r.clone()).await.unwrap();
        let want = truth.get_range(&key, r.clone()).await.unwrap();
        assert_eq!(got, want, "re-read after eviction differs for {r:?}");
    }
}

/// A store whose reads block until the test releases them.
///
/// ⚠️ A test that spawns *n* tasks and trusts the scheduler to overlap them is a lottery, and
/// it fails the way lotteries do — occasionally, for reasons that look like the code under
/// test. A first version used `Barrier::new(1)`, which forces nothing at all: it returns
/// immediately, so whether the callers overlapped was the scheduler's business. It passed
/// normally and **failed under coverage instrumentation**, which is a flaky test announcing
/// itself.
///
/// This blocks instead. The single fetch cannot complete until the test releases it, so by
/// then every caller has provably entered the cache — and a cache that dedupes only
/// sequentially has no way to look correct.
#[derive(Debug)]
struct Barriered {
    inner: MemoryStore,
    gate: tokio::sync::Semaphore,
    reads: std::sync::atomic::AtomicU64,
}

#[async_trait::async_trait]
impl BlobStore for Barriered {
    fn capabilities(&self) -> &pstore_blob::Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, pstore_blob::BlobError> {
        self.inner.get(key).await
    }
    async fn get_range(
        &self,
        key: &Key,
        range: Range<u64>,
    ) -> Result<Bytes, pstore_blob::BlobError> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Held until the test says every caller has arrived.
        self.gate
            .acquire()
            .await
            .map(tokio::sync::SemaphorePermit::forget)
            .ok();
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, pstore_blob::BlobError> {
        self.inner.get_suffix(key, n).await
    }
    async fn head(&self, key: &Key) -> Result<u64, pstore_blob::BlobError> {
        self.inner.head(key).await
    }
    async fn put(
        &self,
        key: &Key,
        body: Bytes,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::BlobError> {
        self.inner.put(key, body).await
    }
    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: pstore_blob::Precondition,
    ) -> Result<pstore_blob::PutOutcome, pstore_blob::CasError> {
        self.inner.put_conditional(key, body, pre).await
    }
    async fn get_with_tag(
        &self,
        key: &Key,
    ) -> Result<(Bytes, pstore_types::CasTag), pstore_blob::BlobError> {
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(
        &self,
        key: &Key,
    ) -> Result<Option<pstore_types::CasTag>, pstore_blob::BlobError> {
        self.inner.get_tag(key).await
    }
    async fn delete_batch(&self, keys: &[Key]) -> Result<(), pstore_blob::BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, pstore_blob::BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_misses_issue_one_request() {
    // ⚠️ The barrier admits ONE at a time and resets, so a broken cache does not hang here —
    // it reports the count. Verified by disabling the in-flight claim: eight racing callers
    // then reached the store twice, and this failed with `left: 2, right: 1`.
    let key = Key::new("seg/hot");
    let inner = MemoryStore::with_coalesce_gap(GAP);
    inner.put(&key, body()).await.unwrap();
    let store = Arc::new(Barriered {
        inner,
        gate: tokio::sync::Semaphore::new(0),
        reads: std::sync::atomic::AtomicU64::new(0),
    });
    let cache = Arc::new(Caching::new(Arc::clone(&store), 1 << 20));

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (c, k) = (Arc::clone(&cache), key.clone());
        tasks.push(tokio::spawn(async move { c.get_range(&k, 0..64).await }));
    }
    // Let all eight enter the cache, then release the one fetch they collapsed into.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    store.gate.add_permits(16);

    for t in tasks {
        let got = t.await.unwrap().unwrap();
        assert_eq!(got.len(), 64, "a racing caller got the wrong bytes");
    }
    assert_eq!(
        store.reads.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "eight concurrent misses on one range should reach the store once"
    );
}

#[tokio::test]
async fn every_uncached_operation_reaches_the_store_unchanged() {
    // ⚠️ A decorator is mostly delegation, and delegation is where a store quietly stops
    // working: a `put_conditional` that drops its precondition breaks CAS — which is this
    // project's only fencing mechanism — while every read test still passes. These are one
    // line each and none of them was covered.
    let key = Key::new("seg/passthrough");
    let inner = Arc::new(MemoryStore::with_coalesce_gap(GAP));
    let cache = Caching::new(Arc::clone(&inner), 1 << 20);

    cache.put(&key, body()).await.unwrap();
    assert_eq!(
        inner.get(&key).await.unwrap(),
        body(),
        "put did not reach the store"
    );
    assert_eq!(cache.head(&key).await.unwrap(), BODY as u64);
    assert_eq!(cache.capabilities().backend, inner.capabilities().backend);

    // CAS, through the decorator: the tag must be the store's, and a stale one must lose.
    let (_, tag) = cache.get_with_tag(&key).await.unwrap();
    assert_eq!(cache.get_tag(&key).await.unwrap().as_ref(), Some(&tag));
    cache
        .put_conditional(
            &key,
            Bytes::from_static(b"second"),
            Precondition::Match(tag.clone()),
        )
        .await
        .expect("a matching precondition must be accepted");
    assert!(
        cache
            .put_conditional(&key, Bytes::from_static(b"third"), Precondition::Match(tag))
            .await
            .is_err(),
        "a stale precondition was accepted, so CAS is not fencing anything"
    );

    let listed = cache.list_unrestricted(&Key::new("seg/")).await.unwrap();
    assert!(listed.contains(&key), "list did not reach the store");

    cache
        .delete_batch(std::slice::from_ref(&key))
        .await
        .unwrap();
    assert!(
        inner.get(&key).await.is_err(),
        "delete did not reach the store"
    );
}

#[tokio::test]
async fn a_cached_suffix_equals_what_the_store_holds() {
    // ⚠️ The earlier suffix test counted requests and never looked at the bytes, so a
    // `get_suffix` returning an empty buffer passed it. Mutation testing found exactly that.
    let (cache, _, key) = fixture(1 << 20).await;
    let truth = MemoryStore::with_coalesce_gap(GAP);
    truth.put(&key, body()).await.unwrap();

    for n in [1u64, 64, 128, 8192, 99_999] {
        let got = cache.get_suffix(&key, n).await.unwrap();
        let want = truth.get_suffix(&key, n).await.unwrap();
        assert_eq!(got, want, "cached suffix of {n} differs from the store");
        // And again, from cache this time.
        assert_eq!(cache.get_suffix(&key, n).await.unwrap(), want);
    }
}

#[tokio::test]
async fn resident_bytes_is_the_bytes_actually_held() {
    // ⚠️ "<= budget and > 0" is satisfied by returning 1. The number has to be the number, or
    // criterion 5 is asserting against a constant.
    let (cache, _, key) = fixture(1 << 20).await;
    assert_eq!(cache.resident_bytes(), 0, "a fresh cache holds nothing");

    cache.get_range(&key, 0..100).await.unwrap();
    assert_eq!(cache.resident_bytes(), 100);
    cache.get_range(&key, 200..350).await.unwrap();
    assert_eq!(cache.resident_bytes(), 250);
    // A repeat admits nothing new.
    cache.get_range(&key, 0..100).await.unwrap();
    assert_eq!(
        cache.resident_bytes(),
        250,
        "a hit changed the resident total"
    );
}

#[tokio::test]
async fn eviction_is_least_recently_used() {
    // ⚠️ Without this, `touch` can be deleted entirely and every other test still passes —
    // the cache would evict its hottest entry and only a hit-rate measurement would notice.
    let (cache, _, key) = fixture(300).await;
    let (a, b, c) = (0..100u64, 1000..1100u64, 2000..2100u64);

    cache.get_range(&key, a.clone()).await.unwrap();
    cache.get_range(&key, b.clone()).await.unwrap();
    cache.get_range(&key, c.clone()).await.unwrap(); // full: 300 of 300
    assert_eq!(cache.resident_bytes(), 300);

    // Touch `a`, making `b` the least recently used.
    cache.get_range(&key, a.clone()).await.unwrap();
    // Admit a fourth; `b` must be the one to go, not `a`.
    cache.get_range(&key, 3000..3100).await.unwrap();

    assert!(
        cache.holds(&key, a),
        "the most recently used entry was evicted"
    );
    assert!(
        !cache.holds(&key, b),
        "the least recently used entry survived"
    );
}

#[tokio::test]
async fn an_entry_larger_than_the_budget_is_refused_not_ruinous() {
    // ⚠️ Admitting it would evict everything and then evict itself, leaving a cache that
    // holds nothing while reporting hits.
    let (cache, _, key) = fixture(512).await;
    cache.get_range(&key, 0..200).await.unwrap();
    assert_eq!(cache.resident_bytes(), 200);

    cache.get_range(&key, 0..4096).await.unwrap(); // four thousand into five hundred

    // ⚠️ And an entry EXACTLY the budget fits. Refusing it wastes the whole arena on nothing,
    // which is the off-by-one mutation testing found here and nothing else caught.
    let (exact, _, k2) = fixture(512).await;
    exact.get_range(&k2, 0..512).await.unwrap();
    assert_eq!(
        exact.resident_bytes(),
        512,
        "an entry exactly the size of the budget was refused, so the arena holds nothing"
    );
    assert_eq!(
        cache.resident_bytes(),
        200,
        "an oversized entry was admitted, evicting what fit"
    );
    assert!(
        cache.holds(&key, 0..200),
        "the oversized read evicted a valid entry"
    );
}

#[tokio::test]
async fn a_repeated_range_within_one_call_fills_both_positions() {
    // ⚠️ `get_ranges` returns one buffer per requested range, positionally. A caller asking
    // for the same range twice — which a probe set with a duplicate list does — must get it
    // in both slots, not once with a hole beside it. Mutation testing reached the slot
    // matching and nothing noticed, because every other test asks for distinct ranges.
    let (cache, _, key) = fixture(1 << 20).await;
    let truth = MemoryStore::with_coalesce_gap(GAP);
    truth.put(&key, body()).await.unwrap();

    let rs = vec![0..64, 2000..2064, 0..64, 4000..4064, 2000..2064];
    let got = cache.get_ranges(&key, &rs).await.unwrap();
    let want = truth.get_ranges(&key, &rs).await.unwrap();
    assert_eq!(got, want, "a duplicated range came back wrong");
    assert_eq!(got[0], got[2], "the two copies of one range differ");
    assert_eq!(got[1], got[4]);

    // And warm, where the duplicates are served from cache rather than assembled.
    assert_eq!(cache.get_ranges(&key, &rs).await.unwrap(), want);
}

// ---------------------------------------------------------------------------------------
// Phase 2 — class-aware admission (D-21)
// ---------------------------------------------------------------------------------------

use pstore_blob::Class;

/// A cache with a small per-class budget, so eviction is easy to provoke.
async fn classed(
    pinned: usize,
    meta: usize,
    bulk: usize,
) -> (
    Caching<TenantView<MemoryStore>>,
    Arc<Accounted<MemoryStore>>,
    Key,
) {
    let acct = Arc::new(Accounted::new(MemoryStore::with_coalesce_gap(GAP)));
    let key = Key::new("seg/classed");
    // Large enough that a sweep of ten times the budget stays inside the object; a read past
    // the end is an error, not a miss.
    let big = Bytes::from((0..32_768).map(|i| (i % 251) as u8).collect::<Vec<u8>>());
    acct.as_tenant(TenantId(1)).put(&key, big).await.unwrap();
    let cache = Caching::with_quotas(Arc::new(acct.as_tenant(TenantId(1))), pinned, meta, bulk);
    (cache, acct, key)
}

#[tokio::test]
async fn bulk_traffic_does_not_evict_metadata() {
    // ⚠️ D-21's entire claim, and the one a plain LRU fails. A byte of centroid table is worth
    // thousands of bytes of raw vectors because every query needs it, so a burst of scan
    // traffic must not be able to walk it out of the cache.
    let (cache, acct, key) = classed(256, 256, 512).await;

    cache
        .get_range_as(&key, 0..128, Class::Pinned)
        .await
        .unwrap();
    cache
        .get_range_as(&key, 200..328, Class::Meta)
        .await
        .unwrap();
    let after_metadata = acct.count(TenantId(1), OpClass::Read);

    // Ten times the WHOLE budget, as bulk.
    for i in 0..80u64 {
        let at = 1000 + i * 128;
        cache
            .get_range_as(&key, at..at + 128, Class::Bulk)
            .await
            .unwrap();
    }
    let after_bulk = acct.count(TenantId(1), OpClass::Read);
    assert!(
        after_bulk > after_metadata,
        "the bulk sweep fetched nothing"
    );

    // Both metadata entries must still be hits: zero further requests.
    cache
        .get_range_as(&key, 0..128, Class::Pinned)
        .await
        .unwrap();
    cache
        .get_range_as(&key, 200..328, Class::Meta)
        .await
        .unwrap();
    assert_eq!(
        acct.count(TenantId(1), OpClass::Read),
        after_bulk,
        "bulk traffic evicted metadata: this is a global LRU wearing a class-aware name"
    );
}

#[tokio::test]
async fn each_class_stays_within_its_quota() {
    // ⚠️ Criterion 8 is satisfiable by never evicting anything at all. The quotas have to
    // bind, or "metadata survived" means "nothing was ever removed".
    let (cache, _, key) = classed(256, 256, 512).await;
    for i in 0..40u64 {
        let at = i * 128;
        cache
            .get_range_as(&key, at..at + 128, Class::Pinned)
            .await
            .unwrap();
        cache
            .get_range_as(&key, at..at + 128, Class::Meta)
            .await
            .unwrap();
        cache
            .get_range_as(&key, at..at + 128, Class::Bulk)
            .await
            .unwrap();
        assert!(cache.resident_in(Class::Pinned) <= 256, "pinned over quota");
        assert!(cache.resident_in(Class::Meta) <= 256, "meta over quota");
        assert!(cache.resident_in(Class::Bulk) <= 512, "bulk over quota");
        assert!(
            cache.resident_bytes() <= 256 + 256 + 512,
            "total over budget"
        );
    }
    assert!(
        cache.resident_in(Class::Bulk) > 0,
        "bulk was evicted to nothing"
    );
}

#[tokio::test]
async fn a_class_hint_survives_a_decorator_stack() {
    // ⚠️ The cache is outermost by design, but tests compose these freely — and a defaulted
    // hint method that a decorator forgets to forward arrives classless and is admitted as
    // `Bulk`, disabling D-21 while every other test still passes.
    use pstore_testkit::depth::DepthCounting;
    let inner = Arc::new(MemoryStore::with_coalesce_gap(GAP));
    let key = Key::new("seg/stacked");
    inner.put(&key, body()).await.unwrap();

    // A decorator ABOVE the cache, which is the layering the hint has to survive. The stack
    // owns the cache, so what it admitted is read back through the stack's own inner handle.
    let stacked = DepthCounting::new(Caching::with_quotas(Arc::clone(&inner), 256, 256, 256));

    stacked
        .get_range_as(&key, 0..128, Class::Pinned)
        .await
        .unwrap();
    let cache = stacked.inner();
    assert_eq!(
        cache.resident_in(Class::Pinned),
        128,
        "the hint was stripped on the way through the stack and admitted as Bulk"
    );
    assert_eq!(cache.resident_in(Class::Bulk), 0);
}

#[tokio::test]
async fn an_immutable_get_is_cached_and_a_plain_get_is_not() {
    // ⚠️ Both halves. `get_immutable` is the caller asserting the object never changes, which
    // is what lets centroids be cached; plain `get` must stay uncached, because the lane
    // registry is read that way and CAS-mutated.
    let (cache, acct, key) = classed(1 << 16, 256, 256).await;

    cache.get_immutable(&key, Class::Pinned).await.unwrap();
    let after = acct.count(TenantId(1), OpClass::Read);
    let first = cache.get_immutable(&key, Class::Pinned).await.unwrap();
    let again = cache.get_immutable(&key, Class::Pinned).await.unwrap();
    assert_eq!(
        acct.count(TenantId(1), OpClass::Read),
        after,
        "an immutable get was not cached, so centroids never will be"
    );
    assert_eq!(
        again, first,
        "the cached whole object differs from the first read"
    );
    assert_eq!(again.len(), 32_768);

    // And the mutable path is untouched.
    cache
        .inner()
        .put(&key, Bytes::from_static(b"changed"))
        .await
        .unwrap();
    assert_eq!(
        cache.get(&key).await.unwrap(),
        Bytes::from_static(b"changed"),
        "a plain `get` was served from cache"
    );
}

#[tokio::test]
async fn opening_a_segment_admits_its_index_as_meta() {
    // ⚠️ Asserted by what the cache HOLDS, never by reading the call site. A caller that
    // hints `Bulk` — or that forgets to hint at all — loses D-21 silently, and the only
    // observable difference is which arena the bytes landed in.
    use pstore_format::{Document, SegmentWriter};

    let inner = Arc::new(MemoryStore::with_coalesce_gap(GAP));
    let key = Key::new("seg/real");
    let mut w = SegmentWriter::new(8);
    for i in 0..64 {
        w.push(Document::new(format!("d{i}"), vec![i as f32, 1.0, 2.0]));
    }
    inner.put(&key, w.finish()).await.unwrap();

    let cache = Caching::with_quotas(Arc::clone(&inner), 1 << 16, 1 << 16, 1 << 16);
    let seg = pstore_format::Segment::open(&cache, &key).await.unwrap();
    assert_eq!(seg.row_count(), 64);

    assert!(
        cache.resident_in(Class::Meta) > 0,
        "opening a segment admitted nothing as Meta: the index section is Bulk, and a scan \
         burst will evict what every query on this segment needs"
    );
    assert_eq!(
        cache.resident_in(Class::Bulk),
        0,
        "opening a segment admitted bulk bytes; open should read metadata only"
    );

    // And a second open is free.
    let before = cache.resident_in(Class::Meta);
    pstore_format::Segment::open(&cache, &key).await.unwrap();
    assert_eq!(cache.resident_in(Class::Meta), before);
}
