//! The decorator itself.

use bytes::Bytes;
use pstore_blob::{BlobError, BlobStore, Capabilities, Class, Key, Precondition, PutOutcome};
use pstore_types::CasTag;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use tokio::sync::Mutex;

/// What a cached answer is stored against.
///
/// ⚠️ The **requested** range, never the fetched one. `get_ranges` coalesces before fetching,
/// so a span is an artefact of which ranges happened to be asked for together; keying on it
/// means two callers wanting the same bytes miss each other.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Id {
    /// An exact byte range of an object.
    Range(String, u64, u64),
    /// A whole object the caller asserted is immutable.
    Whole(String),
    /// The last *n* bytes. ⚠️ A separate shape on purpose: `get_suffix` exists because the
    /// caller does **not** know the object's length, so it cannot be expressed as a range
    /// without the `head` this design refuses to put on the read path.
    Suffix(String, u64),
}

/// The state a cache keeps, shared by every [`Caching`] wrapped over it (M20).
///
/// ⚠️ One core serves every tenant: each cached key names its tenant, so tenants cannot see
/// each other's entries, and one budget is shared rather than divided a million ways.
pub struct CacheCore {
    /// ⚠️ One `State` per class, so a class can only ever evict within itself. **Quotas, not
    /// priorities** (D-21): a priority scheme still lets a large enough bulk burst walk the
    /// metadata out, because the bulk entries keep arriving and something has to go. A quota
    /// cannot, because the bulk arena is the only place bulk pressure is felt.
    pinned: Mutex<State>,
    meta: Mutex<State>,
    bulk: Mutex<State>,
    quota_pinned: usize,
    quota_meta: usize,
    quota_bulk: usize,
    disk: Option<crate::disk::Tiers>,
    state: DiskState,
}

impl std::fmt::Debug for CacheCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheCore")
            .field("quota_pinned", &self.quota_pinned)
            .field("quota_meta", &self.quota_meta)
            .field("quota_bulk", &self.quota_bulk)
            .field("disk", &self.state)
            .finish_non_exhaustive()
    }
}

/// Whether a core has a disk tier, and if it tried to and could not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskState {
    /// Memory only, by choice.
    Off,
    /// The disk tier is open.
    Open,
    /// ⚠️ The directory could not be opened, so the core is memory only. A broken disk
    /// degrades to the store; it never fails a read (`disk-space-management.md`).
    Bypassed(String),
}

/// A read cache over an inner store.
#[derive(Debug)]
pub struct Caching<S> {
    inner: Arc<S>,
    /// ⚠️ `None` forwards every call verbatim, so an uncached server keeps every request
    /// count it had (M20).
    core: Option<Arc<CacheCore>>,
}

/// The memory tier's split of `budget`.
///
/// ⚠️ The shape `cache-hierarchy.md` argues for: the metadata classes are *tiny* -- "<0.1% of
/// a segment" for an index section, "~100 MB per 1B vectors" for centroids -- and are read
/// by every query, so they get a small guaranteed reservation and bulk gets the rest. Giving
/// them a share proportional to their size would be giving them nothing.
pub(crate) fn shares(budget: usize) -> (usize, usize, usize) {
    let pinned = (budget / 10).max(1);
    let meta = (budget / 10).max(1);
    (pinned, meta, budget.saturating_sub(pinned + meta))
}

impl CacheCore {
    /// A memory-only core holding at most `budget` bytes, split by class.
    #[must_use]
    pub fn memory(budget: usize) -> Self {
        let (p, m, b) = shares(budget);
        Self::with_quotas(p, m, b)
    }

    /// A memory-only core with an explicit byte quota per class.
    #[must_use]
    pub fn with_quotas(pinned: usize, meta: usize, bulk: usize) -> Self {
        Self {
            pinned: Mutex::new(State::default()),
            meta: Mutex::new(State::default()),
            bulk: Mutex::new(State::default()),
            quota_pinned: pinned,
            quota_meta: meta,
            quota_bulk: bulk,
            disk: None,
            state: DiskState::Off,
        }
    }

    /// A core of `ram` bytes in memory, with a disk tier behind it (M20).
    ///
    /// ⚠️ **Never fails.** A directory that cannot be opened leaves the core memory only,
    /// reporting [`DiskState::Bypassed`]. A directory filled for another store, or with no
    /// record of which store filled it, is emptied before it serves anything.
    pub async fn open(ram: usize, disk: crate::disk::DiskConfig) -> Self {
        let mut core = Self::memory(ram);
        match crate::disk::Tiers::open(&disk).await {
            Ok(tiers) => {
                core.disk = Some(tiers);
                core.state = DiskState::Open;
            }
            Err(why) => core.state = DiskState::Bypassed(why),
        }
        core
    }

    /// Whether this core has a disk tier.
    #[must_use]
    pub fn disk_state(&self) -> &DiskState {
        &self.state
    }

    /// Flushes the disk tier's writes in flight, and closes it. A core is not used after.
    pub async fn close(&self) {
        if let Some(d) = &self.disk {
            d.close().await;
        }
    }

    fn arena(&self, class: Class) -> (&Mutex<State>, usize) {
        match class {
            Class::Pinned => (&self.pinned, self.quota_pinned),
            Class::Meta => (&self.meta, self.quota_meta),
            // A scan is served from bulk, and never admitted (`admit`).
            Class::Bulk | Class::Scan => (&self.bulk, self.quota_bulk),
        }
    }

    async fn lookup(&self, id: &Id, class: Class) -> Option<Bytes> {
        let (arena, _) = self.arena(class);
        let mut s = arena.lock().await;
        let hit = s.entries.get(id).cloned();
        if hit.is_some() {
            s.touch(id);
        }
        hit
    }

    /// Memory, then disk. A disk hit is promoted into memory.
    ///
    /// ⚠️ The disk read is made **outside** the arena's lock, so one slow disk read never
    /// holds up every other tenant's reads in its class.
    async fn find(&self, id: &Id, class: Class) -> Option<Bytes> {
        if let Some(hit) = self.lookup(id, class).await {
            return Some(hit);
        }
        let hit = self.disk.as_ref()?.get(id, class).await?;
        if class != Class::Scan {
            let (arena, quota) = self.arena(class);
            arena.lock().await.admit(id.clone(), hit.clone(), quota);
        }
        Some(hit)
    }

    /// ⚠️ A [`Class::Scan`] read is never admitted, to either tier (D-50).
    async fn admit(&self, id: Id, bytes: Bytes, class: Class) {
        if class == Class::Scan {
            return;
        }
        if let Some(d) = &self.disk {
            d.insert(&id, &bytes, class);
        }
        let (arena, quota) = self.arena(class);
        arena.lock().await.admit(id, bytes, quota);
    }

    /// Fetch under singleflight: whoever arrives first fetches, the rest wait and then read
    /// the cache.
    ///
    /// ⚠️ Concurrent misses for one range collapsing into one request is called *mandatory,
    /// not optional* by `load-and-hotspots.md`. A stampede must produce slow queries, never a
    /// multiplied load on the store.
    async fn fetch_once<F, Fut>(&self, id: Id, class: Class, fetch: F) -> Result<Bytes, BlobError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Bytes, BlobError>>,
    {
        if let Some(hit) = self.find(&id, class).await {
            return Ok(hit);
        }
        let (arena, _) = self.arena(class);
        // Claim the fetch, or find the claim someone else made.
        let gate = {
            let mut s = arena.lock().await;
            if let Some(g) = s.in_flight.get(&id) {
                Some(Arc::clone(g))
            } else {
                s.in_flight
                    .insert(id.clone(), Arc::new(tokio::sync::Semaphore::new(0)));
                None
            }
        };
        if let Some(gate) = gate {
            // Someone else is fetching. Wait for them, then read what they admitted.
            let _ = gate.acquire().await;
            if let Some(hit) = self.lookup(&id, class).await {
                return Ok(hit);
            }
            // Their fetch failed; ours is now the claim-free path.
        }

        let out = fetch().await;
        if let Ok(bytes) = &out {
            // Admitted before the waiters are released, so they find it.
            self.admit(id.clone(), bytes.clone(), class).await;
        }
        if let Some(gate) = arena.lock().await.in_flight.remove(&id) {
            // ⚠️ Release every waiter, not one. A permit per waiter would strand the rest
            // until the next fetch, which is a deadlock that only appears under contention.
            gate.close();
        }
        out
    }
}

impl<S: BlobStore> Caching<S> {
    /// A cache over `inner`, holding at most `budget` bytes in memory, split by class.
    #[must_use]
    pub fn new(inner: Arc<S>, budget: usize) -> Self {
        Self::over(inner, Some(Arc::new(CacheCore::memory(budget))))
    }

    /// A cache with an explicit byte quota per class.
    #[must_use]
    pub fn with_quotas(inner: Arc<S>, pinned: usize, meta: usize, bulk: usize) -> Self {
        Self::over(
            inner,
            Some(Arc::new(CacheCore::with_quotas(pinned, meta, bulk))),
        )
    }

    /// `inner` read through `core`, which other tenants' views may share; or through nothing
    /// at all.
    #[must_use]
    pub fn over(inner: Arc<S>, core: Option<Arc<CacheCore>>) -> Self {
        Self { inner, core }
    }

    /// Bytes held in memory in one class.
    #[must_use]
    pub fn resident_in(&self, class: Class) -> usize {
        self.core.as_ref().map_or(0, |c| {
            let (arena, _) = c.arena(class);
            arena.try_lock().map_or(0, |s| s.resident)
        })
    }

    /// The store beneath, for a test that needs to change what the cache is caching.
    #[must_use]
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Whether an exact range is resident in memory. For tests that need to see *which*
    /// entry was evicted, which a byte total cannot show.
    #[must_use]
    pub fn holds(&self, key: &Key, range: Range<u64>) -> bool {
        let id = Id::Range(key.as_str().to_owned(), range.start, range.end);
        self.core.as_ref().is_some_and(|c| {
            [&c.pinned, &c.meta, &c.bulk]
                .iter()
                .any(|a| a.try_lock().is_ok_and(|s| s.entries.contains_key(&id)))
        })
    }

    /// Bytes currently held in memory.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.core.as_ref().map_or(0, |c| {
            [&c.pinned, &c.meta, &c.bulk]
                .iter()
                .map(|a| a.try_lock().map_or(0, |s| s.resident))
                .sum()
        })
    }
}

#[derive(Debug, Default)]
struct State {
    entries: HashMap<Id, Bytes>,
    /// Least-recently-used first. ⚠️ Its touch is O(entries), and since M20 one core is shared
    /// by every tenant: a cost, recorded as BACKLOG row 45, not a correctness question.
    order: Vec<Id>,
    resident: usize,
    /// Ranges a task is already fetching, so others wait rather than duplicating the request.
    in_flight: HashMap<Id, Arc<tokio::sync::Semaphore>>,
}

impl State {
    fn touch(&mut self, id: &Id) {
        if let Some(at) = self.order.iter().position(|x| x == id) {
            let owned = self.order.remove(at);
            self.order.push(owned);
        }
    }

    fn admit(&mut self, id: Id, bytes: Bytes, budget: usize) {
        // ⚠️ An entry larger than the whole budget is never admitted. Admitting it would
        // evict everything and then evict itself, which is a cache that holds nothing while
        // reporting a hit rate.
        if bytes.len() > budget {
            return;
        }
        if let Some(old) = self.entries.insert(id.clone(), bytes.clone()) {
            self.resident = self.resident.saturating_sub(old.len());
            self.touch(&id);
        } else {
            self.order.push(id);
        }
        self.resident = self.resident.saturating_add(bytes.len());
        while self.resident > budget {
            let Some(victim) = self.order.first().cloned() else {
                break;
            };
            self.order.remove(0);
            if let Some(gone) = self.entries.remove(&victim) {
                self.resident = self.resident.saturating_sub(gone.len());
            }
        }
    }
}

#[async_trait::async_trait]
impl<S: BlobStore> BlobStore for Caching<S> {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }

    /// ⚠️ **Never cached.** See the module doc: whole-object reads are how a mutable object is
    /// read here, and a stale one loses a lane permanently.
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &Key, range: Range<u64>) -> Result<Bytes, BlobError> {
        if self.core.is_none() {
            return self.inner.get_range(key, range).await;
        }
        self.get_range_as(key, range, Class::default()).await
    }

    async fn get_range_as(
        &self,
        key: &Key,
        range: Range<u64>,
        class: Class,
    ) -> Result<Bytes, BlobError> {
        let Some(core) = &self.core else {
            return self.inner.get_range_as(key, range, class).await;
        };
        let id = Id::Range(key.as_str().to_owned(), range.start, range.end);
        core.fetch_once(id, class, || {
            self.inner.get_range_as(key, range.clone(), class)
        })
        .await
    }

    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        if self.core.is_none() {
            return self.inner.get_ranges(key, ranges).await;
        }
        self.get_ranges_as(key, ranges, Class::default()).await
    }

    async fn get_ranges_as(
        &self,
        key: &Key,
        ranges: &[Range<u64>],
        class: Class,
    ) -> Result<Vec<Bytes>, BlobError> {
        let Some(core) = &self.core else {
            return self.inner.get_ranges_as(key, ranges, class).await;
        };
        let id = |r: &Range<u64>| Id::Range(key.as_str().to_owned(), r.start, r.end);
        // Split hits from misses, keyed by what was ASKED for -- every range looked up at
        // once, since a disk lookup is a read of its own.
        let mut out: Vec<Option<Bytes>> = futures_util::future::join_all(ranges.iter().map(|r| {
            let id = id(r);
            async move { core.find(&id, class).await }
        }))
        .await;
        let missing: Vec<Range<u64>> = ranges
            .iter()
            .zip(&out)
            .filter(|(_, hit)| hit.is_none())
            .map(|(r, _)| r.clone())
            .collect();

        if !missing.is_empty() {
            // ⚠️ The misses go to the inner `get_ranges`, so coalescing still happens — below
            // the cache, where it belongs. The cache decides *what* to fetch; the store
            // decides how few requests that takes.
            let fetched = self.inner.get_ranges_as(key, &missing, class).await?;
            for (r, bytes) in missing.iter().zip(fetched) {
                core.admit(id(r), bytes.clone(), class).await;
                if let Some(slot) = ranges
                    .iter()
                    .zip(out.iter_mut())
                    .find(|(rr, slot)| *rr == r && slot.is_none())
                    .map(|(_, slot)| slot)
                {
                    *slot = Some(bytes);
                }
            }
        }

        out.into_iter()
            .map(|b| {
                b.ok_or_else(|| BlobError::Other("a requested range was never filled".to_owned()))
            })
            .collect()
    }

    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        if self.core.is_none() {
            return self.inner.get_suffix(key, n).await;
        }
        self.get_suffix_as(key, n, Class::default()).await
    }

    async fn get_suffix_as(&self, key: &Key, n: u64, class: Class) -> Result<Bytes, BlobError> {
        let Some(core) = &self.core else {
            return self.inner.get_suffix_as(key, n, class).await;
        };
        let id = Id::Suffix(key.as_str().to_owned(), n);
        core.fetch_once(id, class, || self.inner.get_suffix_as(key, n, class))
            .await
    }

    /// ⚠️ Cached, unlike `get` — because the caller has asserted the object never changes.
    /// That assertion is the whole difference, and it is the caller's to make.
    async fn get_immutable(&self, key: &Key, class: Class) -> Result<Bytes, BlobError> {
        let Some(core) = &self.core else {
            return self.inner.get_immutable(key, class).await;
        };
        let id = Id::Whole(key.as_str().to_owned());
        core.fetch_once(id, class, || self.inner.get_immutable(key, class))
            .await
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
    ) -> Result<PutOutcome, pstore_blob::CasError> {
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
