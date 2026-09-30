//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]

//! The disk tier (M20): a restart is not a flush, another store's directory serves nothing,
//! the class quotas hold on disk, and a broken disk degrades to the store.
//!
//! Every test opens its own directory and closes its cores before reopening one: the tier
//! is written on admission, and `close` is what waits for those writes.

use bytes::Bytes;
use pstore_blob::{Accounted, BlobStore, Class, Key, MemoryStore, OpClass, TenantView};
use pstore_cache::{CacheCore, Caching, DiskConfig, DiskState};
use pstore_types::TenantId;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const MIB: usize = 1 << 20;
const RAM: usize = 64 * MIB;
/// Small blocks, so a test's disk budget holds several per class.
const BLOCK: usize = MIB;
const DISK: usize = 40 * MIB;

/// A fresh directory under the system's temporary one, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "pstore-disk-{}-{name}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(dir: &Path, lane: u64, identity: &str) -> DiskConfig {
    DiskConfig::new(dir, lane, DISK, identity).with_block(BLOCK)
}

async fn open(dir: &Path, identity: &str) -> Arc<CacheCore> {
    Arc::new(CacheCore::open(RAM, config(dir, 1, identity)).await)
}

/// A store with `n` objects `k0..kn`, each `len` bytes that begin with `seed`.
async fn store(n: usize, len: usize, seed: u8) -> Arc<Accounted<MemoryStore>> {
    let acct = Arc::new(Accounted::new(MemoryStore::new()));
    let view = acct.as_tenant(TenantId(1));
    for i in 0..n {
        view.put(&key(i), body(len, seed.wrapping_add(i as u8)))
            .await
            .unwrap();
    }
    acct
}

fn key(i: usize) -> Key {
    Key::new(format!("tnt/1/seg/k{i}.seg"))
}

fn body(len: usize, seed: u8) -> Bytes {
    Bytes::from(
        (0..len)
            .map(|i| seed.wrapping_add((i % 251) as u8))
            .collect::<Vec<u8>>(),
    )
}

fn over(
    acct: &Arc<Accounted<MemoryStore>>,
    core: &Arc<CacheCore>,
) -> Caching<TenantView<MemoryStore>> {
    Caching::over(
        Arc::new(acct.as_tenant(TenantId(1))),
        Some(Arc::clone(core)),
    )
}

fn reads(acct: &Accounted<MemoryStore>) -> u64 {
    acct.count(TenantId(1), OpClass::Read)
}

#[tokio::test]
async fn a_reopened_tier_serves_without_a_request() {
    let dir = Dir::new("reopen");
    let acct = store(1, 8192, 7).await;
    let core = open(&dir.0, "A").await;
    assert_eq!(*core.disk_state(), DiskState::Open);
    let c = over(&acct, &core);
    let range = c.get_range(&key(0), 100..900).await.unwrap();
    let ranges = c
        .get_ranges_as(&key(0), &[0..64, 4096..4160], Class::Meta)
        .await
        .unwrap();
    let suffix = c.get_suffix(&key(0), 512).await.unwrap();
    let whole = c.get_immutable(&key(0), Class::Pinned).await.unwrap();
    core.close().await;

    let core = open(&dir.0, "A").await;
    let c = over(&acct, &core);
    let before = reads(&acct);
    assert_eq!(c.get_range(&key(0), 100..900).await.unwrap(), range);
    assert_eq!(
        c.get_ranges_as(&key(0), &[0..64, 4096..4160], Class::Meta)
            .await
            .unwrap(),
        ranges
    );
    assert_eq!(c.get_suffix(&key(0), 512).await.unwrap(), suffix);
    assert_eq!(
        c.get_immutable(&key(0), Class::Pinned).await.unwrap(),
        whole
    );
    assert_eq!(reads(&acct), before, "a reopened tier went to the store");
    core.close().await;
}

#[tokio::test]
async fn another_stores_directory_serves_nothing() {
    let dir = Dir::new("identity");
    let a = store(1, 4096, 1).await;
    let b = store(1, 4096, 99).await;
    let core = open(&dir.0, "A").await;
    over(&a, &core).get_range(&key(0), 0..4096).await.unwrap();
    core.close().await;

    // Opened for B: B's bytes, at B's cost.
    let core = open(&dir.0, "B").await;
    let before = reads(&b);
    let got = over(&b, &core).get_range(&key(0), 0..4096).await.unwrap();
    assert_eq!(got, body(4096, 99), "B was served A's bytes");
    assert_eq!(reads(&b), before + 1);
    core.close().await;
}

#[tokio::test]
async fn a_tier_emptied_for_another_store_stays_empty() {
    // Opened for B and closed with nothing inserted: A's entries must be gone from the disk,
    // not merely unrecovered, or the next open under a matching identity serves them.
    let dir = Dir::new("emptied");
    let a = store(1, 4096, 1).await;
    let core = open(&dir.0, "A").await;
    over(&a, &core).get_range(&key(0), 0..4096).await.unwrap();
    core.close().await;
    open(&dir.0, "B").await.close().await;
    open(&dir.0, "B").await.close().await;
    // Back under A: nothing of A's survived B's emptying.
    let core = open(&dir.0, "A").await;
    let before = reads(&a);
    over(&a, &core).get_range(&key(0), 0..4096).await.unwrap();
    assert_eq!(reads(&a), before + 1, "an entry survived the emptying");
    core.close().await;
}

#[tokio::test]
async fn a_missing_identity_empties_the_tier() {
    let dir = Dir::new("missing");
    let a = store(1, 4096, 1).await;
    let core = open(&dir.0, "A").await;
    over(&a, &core).get_range(&key(0), 0..4096).await.unwrap();
    core.close().await;
    let identity = dir.0.join("lane-1").join("identity");
    assert!(identity.is_file(), "no identity at {identity:?}");
    std::fs::remove_file(&identity).unwrap();
    let core = open(&dir.0, "A").await;
    let before = reads(&a);
    over(&a, &core).get_range(&key(0), 0..4096).await.unwrap();
    assert_eq!(reads(&a), before + 1, "served with no identity to check");
    core.close().await;
}

#[tokio::test]
async fn a_bulk_burst_leaves_meta_on_disk() {
    let dir = Dir::new("quota");
    // The bulk share is 32 MiB of the 40; the burst is twice that, in quarter-block entries.
    let n = 2 * 32 * 4;
    let acct = store(n + 1, BLOCK / 4 - 4096, 3).await;
    let core = open(&dir.0, "A").await;
    let c = over(&acct, &core);
    let meta = c.get_range_as(&key(n), 0..4096, Class::Meta).await.unwrap();
    for i in 0..n {
        c.get_range_as(&key(i), 0..(BLOCK as u64 / 4 - 4096), Class::Bulk)
            .await
            .unwrap();
    }
    core.close().await;
    let core = open(&dir.0, "A").await;
    let c = over(&acct, &core);
    let before = reads(&acct);
    assert_eq!(
        c.get_range_as(&key(n), 0..4096, Class::Meta).await.unwrap(),
        meta
    );
    assert_eq!(reads(&acct), before, "the bulk burst evicted meta on disk");
    // And the burst did evict: its first entries are gone.
    c.get_range_as(&key(0), 0..(BLOCK as u64 / 4 - 4096), Class::Bulk)
        .await
        .unwrap();
    assert_eq!(reads(&acct), before + 1, "the burst evicted nothing");
    core.close().await;
}

/// Every regular file under `dir`, recursively.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn a_flipped_byte_is_a_miss() {
    let dir = Dir::new("flip");
    let acct = store(1, 8192, 11).await;
    let core = open(&dir.0, "A").await;
    let want = over(&acct, &core)
        .get_range(&key(0), 0..8192)
        .await
        .unwrap();
    core.close().await;
    // Find the value itself in the tier's files -- a byte flipped in padding proves nothing.
    let mut flipped = 0;
    for f in files(&dir.0) {
        let mut data = std::fs::read(&f).unwrap();
        if let Some(at) = data.windows(want.len()).position(|w| w == &want[..]) {
            data[at + 4000] ^= 0xff;
            std::fs::write(&f, &data).unwrap();
            flipped += 1;
        }
    }
    assert_eq!(flipped, 1, "the value was not found once on disk");
    let core = open(&dir.0, "A").await;
    let before = reads(&acct);
    let got = over(&acct, &core)
        .get_range(&key(0), 0..8192)
        .await
        .unwrap();
    assert_eq!(got, want, "a corrupt entry was served");
    assert_eq!(reads(&acct), before + 1);
    core.close().await;
}

#[tokio::test]
async fn a_directory_that_is_a_file_bypasses() {
    let dir = Dir::new("file");
    std::fs::write(&dir.0, b"not a directory").unwrap();
    let acct = store(1, 4096, 5).await;
    let core = open(&dir.0, "A").await;
    assert!(
        matches!(core.disk_state(), DiskState::Bypassed(_)),
        "{:?}",
        core.disk_state()
    );
    let before = reads(&acct);
    let got = over(&acct, &core)
        .get_range(&key(0), 0..4096)
        .await
        .unwrap();
    assert_eq!(got, body(4096, 5));
    assert_eq!(reads(&acct), before + 1);
    core.close().await;
    // Nothing reached a disk, so a new core pays again.
    let core = open(&dir.0, "A").await;
    over(&acct, &core)
        .get_range(&key(0), 0..4096)
        .await
        .unwrap();
    assert_eq!(reads(&acct), before + 2);
    let _ = std::fs::remove_file(&dir.0);
}

#[tokio::test]
async fn truncated_tier_files_read_correctly() {
    let dir = Dir::new("truncated");
    let acct = store(4, 8192, 13).await;
    let core = open(&dir.0, "A").await;
    for i in 0..4 {
        over(&acct, &core)
            .get_range(&key(i), 0..8192)
            .await
            .unwrap();
    }
    core.close().await;
    // Reopened (the entries are indexed), then the data truncated under the open core.
    let core = open(&dir.0, "A").await;
    for f in files(&dir.0) {
        if f.file_name().is_some_and(|n| n != "identity") {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&f)
                .unwrap()
                .set_len(0)
                .unwrap();
        }
    }
    let c = over(&acct, &core);
    for i in 0..4 {
        assert_eq!(
            c.get_range(&key(i), 0..8192).await.unwrap(),
            body(8192, 13u8.wrapping_add(i as u8))
        );
    }
    core.close().await;
}

#[tokio::test]
async fn get_is_never_cached_on_disk() {
    let dir = Dir::new("get");
    let acct = store(1, 4096, 17).await;
    let core = open(&dir.0, "A").await;
    let before = reads(&acct);
    over(&acct, &core).get(&key(0)).await.unwrap();
    core.close().await;
    let core = open(&dir.0, "A").await;
    over(&acct, &core).get(&key(0)).await.unwrap();
    assert_eq!(reads(&acct), before + 2);
    core.close().await;
}

#[tokio::test]
async fn a_range_a_suffix_and_a_whole_are_three_entries() {
    let dir = Dir::new("ids");
    let acct = store(1, 4096, 19).await;
    let all = body(4096, 19);
    let core = open(&dir.0, "A").await;
    let c = over(&acct, &core);
    c.get_range(&key(0), 0..16).await.unwrap();
    c.get_suffix(&key(0), 16).await.unwrap();
    c.get_immutable(&key(0), Class::Bulk).await.unwrap();
    assert_eq!(reads(&acct), 3);
    core.close().await;
    let core = open(&dir.0, "A").await;
    let c = over(&acct, &core);
    assert_eq!(c.get_range(&key(0), 0..16).await.unwrap(), all.slice(0..16));
    assert_eq!(c.get_suffix(&key(0), 16).await.unwrap(), all.slice(4080..));
    assert_eq!(c.get_immutable(&key(0), Class::Bulk).await.unwrap(), all);
    assert_eq!(reads(&acct), 3, "a reopened entry went to the store");
    core.close().await;
}

#[tokio::test]
async fn an_entry_larger_than_a_block_stays_in_memory() {
    let dir = Dir::new("large");
    let len = 4 * MIB + 1;
    let acct = store(1, len, 23).await;
    let core = Arc::new(CacheCore::open(RAM, DiskConfig::new(&dir.0, 1, 64 * MIB, "A")).await);
    let c = over(&acct, &core);
    let want = c.get_range(&key(0), 0..len as u64).await.unwrap();
    assert_eq!(c.get_range(&key(0), 0..len as u64).await.unwrap(), want);
    assert_eq!(reads(&acct), 1, "not a memory hit");
    core.close().await;
    let core = Arc::new(CacheCore::open(RAM, DiskConfig::new(&dir.0, 1, 64 * MIB, "A")).await);
    let got = over(&acct, &core)
        .get_range(&key(0), 0..len as u64)
        .await
        .unwrap();
    assert_eq!(got, want);
    assert_eq!(reads(&acct), 2, "a block-sized entry reached the disk");
    core.close().await;
}

#[tokio::test]
async fn lanes_do_not_share_a_tier() {
    let dir = Dir::new("lanes");
    let acct = store(1, 4096, 29).await;
    let one = Arc::new(CacheCore::open(RAM, config(&dir.0, 1, "A")).await);
    let two = Arc::new(CacheCore::open(RAM, config(&dir.0, 2, "A")).await);
    over(&acct, &one).get_range(&key(0), 0..4096).await.unwrap();
    one.close().await;
    two.close().await;
    let two = Arc::new(CacheCore::open(RAM, config(&dir.0, 2, "A")).await);
    let before = reads(&acct);
    over(&acct, &two).get_range(&key(0), 0..4096).await.unwrap();
    assert_eq!(reads(&acct), before + 1, "lane 2 was served lane 1's entry");
    two.close().await;
}

#[tokio::test]
async fn without_a_core_every_read_goes_to_the_store() {
    let acct = store(1, 4096, 31).await;
    let c = Caching::over(Arc::new(acct.as_tenant(TenantId(1))), None);
    for _ in 0..2 {
        c.get_range(&key(0), 0..64).await.unwrap();
        c.get_suffix(&key(0), 64).await.unwrap();
        c.get_immutable(&key(0), Class::Pinned).await.unwrap();
        c.get_ranges(&key(0), &[0..8, 2048..2056]).await.unwrap();
    }
    // Four calls a round; `get_ranges`' two ranges are far enough apart to be two reads, or
    // close enough to be one -- whichever `MemoryStore` does, it does it twice.
    let per_round = reads(&acct) / 2;
    assert!(per_round >= 4, "{per_round}");
    assert_eq!(reads(&acct), 2 * per_round);
}

#[tokio::test]
#[ignore = "wall-clock: run by hand with --ignored; the number is provisional"]
async fn a_full_tier_reopens_within_a_second() {
    let dir = Dir::new("timing");
    let entries = 256 * 4;
    let acct = store(entries, MIB / 4 - 4096, 37).await;
    let cfg = || DiskConfig::new(&dir.0, 1, 256 * MIB, "A");
    let core = Arc::new(CacheCore::open(RAM, cfg()).await);
    for i in 0..entries {
        over(&acct, &core)
            .get_range(&key(i), 0..(MIB as u64 / 4 - 4096))
            .await
            .unwrap();
    }
    core.close().await;
    let at = std::time::Instant::now();
    let core = CacheCore::open(RAM, cfg()).await;
    let took = at.elapsed();
    core.close().await;
    println!("reopened a 256 MiB tier in {took:?} (provisional)");
    assert!(took < std::time::Duration::from_secs(1), "{took:?}");
}

/// A store that records which read method it was called by, for the core-less forwarding.
#[derive(Default)]
struct Recording {
    inner: MemoryStore,
    calls: std::sync::Mutex<Vec<&'static str>>,
}

impl Recording {
    fn saw(&self, what: &'static str) {
        self.calls.lock().unwrap().push(what);
    }
}

#[async_trait::async_trait]
impl BlobStore for Recording {
    fn capabilities(&self) -> &pstore_blob::Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, pstore_blob::BlobError> {
        self.saw("get");
        self.inner.get(key).await
    }
    async fn get_range(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
    ) -> Result<Bytes, pstore_blob::BlobError> {
        self.saw("get_range");
        self.inner.get_range(key, range).await
    }
    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: Class,
    ) -> Result<Bytes, pstore_blob::BlobError> {
        self.saw("get_range_as");
        self.inner.get_range_as(key, range, class).await
    }
    async fn get_ranges(
        &self,
        key: &Key,
        ranges: &[std::ops::Range<u64>],
    ) -> Result<Vec<Bytes>, pstore_blob::BlobError> {
        self.saw("get_ranges");
        self.inner.get_ranges(key, ranges).await
    }
    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[std::ops::Range<u64>],
        class: Class,
    ) -> Result<Vec<Bytes>, pstore_blob::BlobError> {
        self.saw("get_ranges_as");
        self.inner.get_ranges_as(key, ranges, class).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, pstore_blob::BlobError> {
        self.saw("get_suffix");
        self.inner.get_suffix(key, n).await
    }
    async fn get_suffix_as(
        &self,
        key: &Key,
        n: u64,
        class: Class,
    ) -> Result<Bytes, pstore_blob::BlobError> {
        self.saw("get_suffix_as");
        self.inner.get_suffix_as(key, n, class).await
    }
    async fn get_immutable(
        &self,
        key: &Key,
        class: Class,
    ) -> Result<Bytes, pstore_blob::BlobError> {
        self.saw("get_immutable");
        self.inner.get_immutable(key, class).await
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

#[tokio::test]
async fn without_a_core_each_call_is_forwarded_as_itself() {
    // Verbatim: an uncached server must reach its store by the same calls it always did.
    let inner = Arc::new(Recording::default());
    inner.put(&key(0), body(4096, 41)).await.unwrap();
    let c = Caching::over(Arc::clone(&inner), None);
    c.get_range(&key(0), 0..8).await.unwrap();
    c.get_range_as(&key(0), 0..8, Class::Meta).await.unwrap();
    c.get_ranges(&key(0), &[0..8, 16..24]).await.unwrap();
    c.get_ranges_as(&key(0), &[0..8, 16..24], Class::Meta)
        .await
        .unwrap();
    c.get_suffix(&key(0), 8).await.unwrap();
    c.get_suffix_as(&key(0), 8, Class::Meta).await.unwrap();
    c.get_immutable(&key(0), Class::Pinned).await.unwrap();
    c.get(&key(0)).await.unwrap();
    assert_eq!(
        *inner.calls.lock().unwrap(),
        [
            "get_range",
            "get_range_as",
            "get_ranges",
            "get_ranges_as",
            "get_suffix",
            "get_suffix_as",
            "get_immutable",
            "get"
        ]
    );
}

#[tokio::test]
async fn an_empty_range_and_a_whole_object_do_not_collide() {
    // Keyed structurally: without the shape's tag, `Range(k, 0, 0)` and `Whole(k)` encode
    // alike, and the whole object would come back empty.
    let dir = Dir::new("collide");
    let acct = store(1, 4096, 43).await;
    let core = open(&dir.0, "A").await;
    let c = over(&acct, &core);
    assert!(c.get_range(&key(0), 0..0).await.unwrap().is_empty());
    c.get_immutable(&key(0), Class::Bulk).await.unwrap();
    core.close().await;
    let core = open(&dir.0, "A").await;
    let c = over(&acct, &core);
    assert!(c.get_range(&key(0), 0..0).await.unwrap().is_empty());
    assert_eq!(
        c.get_immutable(&key(0), Class::Bulk).await.unwrap(),
        body(4096, 43)
    );
    core.close().await;
}

#[tokio::test]
async fn a_tier_of_another_format_is_emptied() {
    // A directory whose `identity` records the right store but no format -- or another one --
    // was written by a different entry encoding, and must not be decoded as this one's.
    let dir = Dir::new("format");
    let acct = store(1, 4096, 47).await;
    let core = open(&dir.0, "A").await;
    over(&acct, &core)
        .get_range(&key(0), 0..4096)
        .await
        .unwrap();
    core.close().await;
    std::fs::write(dir.0.join("lane-1").join("identity"), "A").unwrap();
    let core = open(&dir.0, "A").await;
    let before = reads(&acct);
    over(&acct, &core)
        .get_range(&key(0), 0..4096)
        .await
        .unwrap();
    assert_eq!(
        reads(&acct),
        before + 1,
        "another format's entry was served"
    );
    core.close().await;
}

#[tokio::test]
async fn a_scan_is_served_from_bulk_and_admits_nothing() {
    // D-50: a scan's miss is fetched and not admitted, to either tier; its hit is served from
    // what a bulk read admitted.
    let dir = Dir::new("scan");
    let acct = store(2, 4096, 53).await;
    let core = open(&dir.0, "A").await;
    let c = over(&acct, &core);
    c.get_range_as(&key(0), 0..512, Class::Bulk).await.unwrap();
    let before = (reads(&acct), c.resident_in(Class::Bulk));
    assert_eq!(
        c.get_range_as(&key(0), 0..512, Class::Scan).await.unwrap(),
        body(4096, 53).slice(0..512)
    );
    c.get_ranges_as(&key(0), std::slice::from_ref(&(0..512)), Class::Scan)
        .await
        .unwrap();
    assert_eq!(
        (reads(&acct), c.resident_in(Class::Bulk)),
        before,
        "a scan missed bulk"
    );
    // A miss: fetched, and nowhere afterwards.
    c.get_range_as(&key(1), 0..512, Class::Scan).await.unwrap();
    c.get_ranges_as(&key(1), &[1024..1536, 2048..2560], Class::Scan)
        .await
        .unwrap();
    assert_eq!(c.resident_in(Class::Bulk), before.1, "a scan was admitted");
    core.close().await;
    let core = open(&dir.0, "A").await;
    let c = over(&acct, &core);
    let r = reads(&acct);
    c.get_range_as(&key(1), 0..512, Class::Bulk).await.unwrap();
    assert_eq!(reads(&acct), r + 1, "a scan reached the disk");
    // A scan served from disk is not promoted into memory either.
    let held = c.resident_in(Class::Bulk);
    c.get_range_as(&key(0), 0..512, Class::Scan).await.unwrap();
    assert_eq!(reads(&acct), r + 1, "a scan missed the disk's bulk");
    assert_eq!(
        c.resident_in(Class::Bulk),
        held,
        "a scan's disk hit was promoted"
    );
    core.close().await;
}

#[tokio::test]
async fn a_scan_is_served_from_the_memory_tiers_bulk() {
    // Memory only, so a disk cannot answer for a memory tier that looked in the wrong place.
    let acct = store(1, 4096, 59).await;
    let core = Arc::new(CacheCore::memory(RAM));
    let c = over(&acct, &core);
    c.get_range_as(&key(0), 0..512, Class::Bulk).await.unwrap();
    let r = reads(&acct);
    c.get_range_as(&key(0), 0..512, Class::Scan).await.unwrap();
    c.get_ranges_as(&key(0), std::slice::from_ref(&(0..512)), Class::Scan)
        .await
        .unwrap();
    assert_eq!(reads(&acct), r, "a scan missed the memory tier's bulk");
}
