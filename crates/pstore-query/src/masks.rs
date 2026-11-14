//! A server's filter-mask cache (M234): `(segment, filter)` to the segment's mask.
//!
//! ⚠️ **Never stale, so never invalidated.** A segment is immutable and its key is never
//! reused, and a mask holds no deletes (they are applied after it), so an entry is right for
//! as long as it is held. Nothing durable, and an answer never depends on it: a miss computes
//! what a hit would have returned.
//!
//! Read on the query's own task, as M165's plan is: [`scope`] runs a query with a cache, and
//! the query looks each segment's mask up before spawning the task that would build it.
//! Bounded in bytes, least recently used evicted first. One cache serves every tenant on a
//! server: each [`scope`] names its tenant, and the name is part of every key.

use crate::filter::Mask;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

/// What an entry costs beside its mask's bytes and its key's: the maps' bookkeeping.
const OVERHEAD: usize = 96;

/// `(tenant, segment key, the filter's canonical text)`.
type EntryKey = (String, String, String);

/// The cache: a byte budget, and the entries it holds.
#[derive(Debug)]
pub struct MaskCache {
    budget: usize,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Each entry's mask, last use and cost.
    entries: HashMap<EntryKey, (Arc<Mask>, u64, usize)>,
    /// Entries by last use, oldest first.
    order: BTreeMap<u64, EntryKey>,
    tick: u64,
    held: usize,
}

impl MaskCache {
    /// A cache holding at most `budget` bytes; `0` holds nothing.
    #[must_use]
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Its budget, in bytes.
    #[must_use]
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// The bytes it holds: never above [`Self::budget`].
    #[must_use]
    pub fn held(&self) -> usize {
        self.lock().held
    }

    /// How many masks it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // ⚠️ Recovered, not dropped: a panic mid-update leaves at worst an entry counted and
        // not held, and the cache stays bounded.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The mask held for `key`, now its most recently used.
    pub(crate) fn get(&self, key: &EntryKey) -> Option<Arc<Mask>> {
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        let (mask, last) = {
            let (mask, last, _) = inner.entries.get_mut(key)?;
            let was = *last;
            *last = tick;
            (Arc::clone(mask), was)
        };
        if let Some(k) = inner.order.remove(&last) {
            inner.order.insert(tick, k);
        }
        Some(mask)
    }

    /// Holds `mask` for `key`, evicting the least recently used until it fits. A mask costing
    /// more than an eighth of the budget is not held: one would evict most of the others.
    pub(crate) fn insert(&self, key: EntryKey, mask: Arc<Mask>) {
        let cost = mask.len() + key.0.len() + key.1.len() + key.2.len() + OVERHEAD;
        if cost > self.budget / 8 {
            return;
        }
        let mut inner = self.lock();
        if inner.entries.contains_key(&key) {
            return;
        }
        while inner.held + cost > self.budget {
            let Some((_, oldest)) = inner.order.pop_first() else {
                break;
            };
            if let Some((_, _, c)) = inner.entries.remove(&oldest) {
                inner.held -= c;
            }
        }
        inner.tick += 1;
        let tick = inner.tick;
        inner.order.insert(tick, key.clone());
        inner.entries.insert(key, (mask, tick, cost));
        inner.held += cost;
    }
}

/// The cache a query runs with, and the tenant it names in each key.
#[derive(Debug, Clone)]
pub struct Scope {
    /// The cache.
    pub cache: Arc<MaskCache>,
    /// The tenant, as the keys name it.
    pub tenant: String,
}

tokio::task_local! {
    static SCOPE: Option<Scope>;
}

/// Runs `fut` with `scope`'s cache for the filter masks of the queries it runs; with `None`,
/// as if no cache existed.
pub async fn scope<F: std::future::Future>(scope: Option<Scope>, fut: F) -> F::Output {
    SCOPE.scope(scope, fut).await
}

/// The cache of the query on this task, if it runs with one.
pub(crate) fn current() -> Option<Scope> {
    SCOPE.try_with(Clone::clone).ok().flatten()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn key(n: usize) -> EntryKey {
        ("1".to_owned(), format!("seg/{n}"), "f".to_owned())
    }

    /// A mask of `rows` rows admitting the even ones.
    fn mask(rows: usize) -> Arc<Mask> {
        Arc::new((0..rows).filter(|r| r % 2 == 0).collect())
    }

    fn cost(rows: usize, n: usize) -> usize {
        let k = key(n);
        mask(rows).len() + k.0.len() + k.1.len() + k.2.len() + OVERHEAD
    }

    #[test]
    fn a_hit_returns_the_mask_held_and_its_key_alone() {
        let c = MaskCache::new(1 << 20);
        let m = mask(100);
        c.insert(key(1), Arc::clone(&m));
        assert!(Arc::ptr_eq(&c.get(&key(1)).unwrap(), &m));
        assert!(c.get(&key(2)).is_none());
        let other_tenant = ("2".to_owned(), "seg/1".to_owned(), "f".to_owned());
        assert!(c.get(&other_tenant).is_none());
        let other_filter = ("1".to_owned(), "seg/1".to_owned(), "g".to_owned());
        assert!(c.get(&other_filter).is_none());
        assert_eq!(c.held(), cost(100, 1));
    }

    #[test]
    fn the_least_recently_used_goes_first() {
        // Room for eight entries of 1,000 rows, the most an eighth each allows.
        let c = MaskCache::new(cost(999, 1) * 8 + cost(999, 1) / 2);
        for n in 0..8 {
            c.insert(key(n), mask(999));
        }
        assert_eq!(c.len(), 8);
        // Used: 0 is now newer than 1.
        assert!(c.get(&key(0)).is_some());
        c.insert(key(8), mask(999));
        assert_eq!(c.len(), 8);
        assert!(c.get(&key(1)).is_none(), "the least recently used stays");
        for n in [0, 2, 3, 4, 5, 6, 7, 8] {
            assert!(c.get(&key(n)).is_some(), "{n} evicted");
        }
        assert!(c.held() <= c.budget());
    }

    /// Two hits in a row each become the newest, and every entry stays evictable (CI's
    /// mutation review of #59: `get`'s `tick += 1` as `*=` gave both hits one tick, so the
    /// second overwrote the first's place in the order and orphaned two entries for good).
    #[test]
    fn hits_in_a_row_keep_every_entry_evictable() {
        let c = MaskCache::new(cost(999, 1) * 8 + cost(999, 1) / 2);
        for n in 0..8 {
            c.insert(key(n), mask(999));
        }
        assert!(c.get(&key(0)).is_some());
        assert!(c.get(&key(1)).is_some());
        // Seven more evict 2..=7, then 0, the older of the two hits.
        for n in 8..15 {
            c.insert(key(n), mask(999));
        }
        assert_eq!(c.len(), 8);
        for n in [0, 2, 3, 4, 5, 6, 7] {
            assert!(c.get(&key(n)).is_none(), "{n} held");
        }
        for n in [1, 8, 9, 10, 11, 12, 13, 14] {
            assert!(c.get(&key(n)).is_some(), "{n} evicted");
        }
    }

    #[test]
    fn a_large_mask_and_a_zero_budget_keep_nothing() {
        let c = MaskCache::new(8 * cost(1000, 1));
        c.insert(key(1), mask(1000));
        assert_eq!(c.len(), 1);
        c.insert(key(2), mask(1100));
        assert!(c.get(&key(2)).is_none(), "over an eighth is not held");
        let none = MaskCache::new(0);
        none.insert(key(1), mask(1));
        assert!(none.is_empty());
        assert_eq!(none.held(), 0);
    }

    #[test]
    fn bytes_held_never_pass_the_budget() {
        let c = MaskCache::new(20_000);
        for n in 0..500 {
            c.insert(key(n), mask(100 + n * 3 % 1_900));
            assert!(c.held() <= c.budget(), "{n}: {} held", c.held());
        }
        // A second insert of a held key changes nothing.
        let before = c.held();
        c.insert(key(499), mask(10));
        assert_eq!(c.held(), before);
    }
}
