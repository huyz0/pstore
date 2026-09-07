//! The decorator itself.

use bytes::Bytes;
use pstore_blob::{BlobError, BlobStore, Capabilities, Key, Precondition, PutOutcome};
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
enum Id {
    /// An exact byte range of an object.
    Range(String, u64, u64),
    /// The last *n* bytes. ⚠️ A separate shape on purpose: `get_suffix` exists because the
    /// caller does **not** know the object's length, so it cannot be expressed as a range
    /// without the `head` this design refuses to put on the read path.
    Suffix(String, u64),
}

/// A read cache over an inner store.
#[derive(Debug)]
pub struct Caching<S> {
    inner: Arc<S>,
    state: Mutex<State>,
    budget: usize,
}

#[derive(Debug, Default)]
struct State {
    entries: HashMap<Id, Bytes>,
    /// Least-recently-used first. Small fleets, small caches; a vector is honest here and a
    /// linked list would be a lie about how much this has been optimised.
    order: Vec<Id>,
    resident: usize,
    /// Ranges a task is already fetching, so others wait rather than duplicating the request.
    in_flight: HashMap<Id, Arc<tokio::sync::Semaphore>>,
}

impl<S: BlobStore> Caching<S> {
    /// A cache over `inner`, holding at most `budget` bytes.
    #[must_use]
    pub fn new(inner: Arc<S>, budget: usize) -> Self {
        Self {
            inner,
            state: Mutex::new(State::default()),
            budget,
        }
    }

    /// The store beneath, for a test that needs to change what the cache is caching.
    #[must_use]
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Whether an exact range is resident. For tests that need to see *which* entry was
    /// evicted, which a byte total cannot show.
    #[must_use]
    pub fn holds(&self, key: &Key, range: Range<u64>) -> bool {
        let id = Id::Range(key.as_str().to_owned(), range.start, range.end);
        self.state
            .try_lock()
            .is_ok_and(|s| s.entries.contains_key(&id))
    }

    /// Bytes currently held.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.state.try_lock().map_or(0, |s| s.resident)
    }

    async fn lookup(&self, id: &Id) -> Option<Bytes> {
        let mut s = self.state.lock().await;
        let hit = s.entries.get(id).cloned();
        if hit.is_some() {
            s.touch(id);
        }
        hit
    }

    async fn admit(&self, id: Id, bytes: Bytes) {
        let mut s = self.state.lock().await;
        s.admit(id, bytes, self.budget);
    }

    /// Fetch under singleflight: whoever arrives first fetches, the rest wait and then read
    /// the cache.
    ///
    /// ⚠️ Concurrent misses for one range collapsing into one request is called *mandatory,
    /// not optional* by `load-and-hotspots.md`. A stampede must produce slow queries, never a
    /// multiplied load on the store.
    async fn fetch_once<F, Fut>(&self, id: Id, fetch: F) -> Result<Bytes, BlobError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Bytes, BlobError>>,
    {
        if let Some(hit) = self.lookup(&id).await {
            return Ok(hit);
        }
        // Claim the fetch, or find the claim someone else made.
        let gate = {
            let mut s = self.state.lock().await;
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
            if let Some(hit) = self.lookup(&id).await {
                return Ok(hit);
            }
            // Their fetch failed; ours is now the claim-free path.
        }

        let out = fetch().await;
        let mut s = self.state.lock().await;
        if let Some(gate) = s.in_flight.remove(&id) {
            // ⚠️ Release every waiter, not one. A permit per waiter would strand the rest
            // until the next fetch, which is a deadlock that only appears under contention.
            gate.close();
        }
        if let Ok(bytes) = &out {
            s.admit(id, bytes.clone(), self.budget);
        }
        out
    }
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
        let id = Id::Range(key.as_str().to_owned(), range.start, range.end);
        self.fetch_once(id, || self.inner.get_range(key, range.clone()))
            .await
    }

    async fn get_ranges(&self, key: &Key, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, BlobError> {
        // Split hits from misses, keyed by what was ASKED for.
        let mut out: Vec<Option<Bytes>> = Vec::with_capacity(ranges.len());
        let mut missing: Vec<Range<u64>> = Vec::new();
        for r in ranges {
            let id = Id::Range(key.as_str().to_owned(), r.start, r.end);
            let hit = self.lookup(&id).await;
            if hit.is_none() {
                missing.push(r.clone());
            }
            out.push(hit);
        }

        if !missing.is_empty() {
            // ⚠️ The misses go to the inner `get_ranges`, so coalescing still happens — below
            // the cache, where it belongs. The cache decides *what* to fetch; the store
            // decides how few requests that takes.
            let fetched = self.inner.get_ranges(key, &missing).await?;
            for (r, bytes) in missing.iter().zip(fetched) {
                self.admit(
                    Id::Range(key.as_str().to_owned(), r.start, r.end),
                    bytes.clone(),
                )
                .await;
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
        let id = Id::Suffix(key.as_str().to_owned(), n);
        self.fetch_once(id, || self.inner.get_suffix(key, n)).await
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

    async fn get_tag(&self, key: &Key) -> Option<CasTag> {
        self.inner.get_tag(key).await
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }
}
