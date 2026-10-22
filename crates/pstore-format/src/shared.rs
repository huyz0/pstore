//! Decoded immutable objects, kept by the buffer they were decoded from (M122).
//!
//! ⚠️ **Keyed by the buffer, never by a name.** A term dictionary is an immutable object, and
//! the pinned cache hands back the same `Bytes` for it on every hit -- so the buffer's address
//! and length name what was decoded, in every store at once: two stores holding an object
//! under the same key never meet here. The entry holds a clone of that `Bytes`, so its memory
//! cannot be freed and reused under the key while the key names it. A buffer of the same bytes
//! elsewhere decodes again, which is the cost today's code paid on every read; a refusal is
//! never kept, so it is judged again each time, word for word.

use bytes::Bytes;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

/// A buffer's identity: where its bytes are, and how many.
type Ident = (usize, usize);

/// Decoded values by the buffer they came from, the oldest evicted past `budget` bytes.
pub(crate) struct ByBuffer<T> {
    state: Mutex<State<T>>,
    budget: usize,
}

struct State<T> {
    entries: HashMap<Ident, Entry<T>>,
    /// Last use -> the entry, so the oldest is the first.
    order: BTreeMap<u64, Ident>,
    used: usize,
    tick: u64,
}

struct Entry<T> {
    /// Held so the address cannot be reused while it is a key.
    _raw: Bytes,
    value: Arc<T>,
    /// What it keeps alive: the buffer, and the decoded value's heap.
    cost: usize,
    at: u64,
}

impl<T> ByBuffer<T> {
    /// An empty cache keeping at most `budget` bytes alive.
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            state: Mutex::new(State {
                entries: HashMap::new(),
                order: BTreeMap::new(),
                used: 0,
                tick: 0,
            }),
            budget,
        }
    }

    /// `raw` decoded: the value kept for this very buffer, or `decode`'s, kept when it fits.
    /// `None` when `decode` refuses -- refused again next time, never remembered.
    pub(crate) fn get_or(
        &self,
        raw: &Bytes,
        size: impl Fn(&T) -> usize,
        decode: impl FnOnce(&Bytes) -> Option<T>,
    ) -> Option<Arc<T>> {
        let id: Ident = (raw.as_ptr() as usize, raw.len());
        {
            let mut s = self.locked();
            let tick = s.tick + 1;
            if let Some(e) = s.entries.get_mut(&id) {
                let (value, then) = (Arc::clone(&e.value), std::mem::replace(&mut e.at, tick));
                s.tick = tick;
                s.order.remove(&then);
                s.order.insert(tick, id);
                return Some(value);
            }
        }
        // Decoded outside the lock: a large dictionary never stalls another's hit.
        let value = Arc::new(decode(raw)?);
        let cost = raw.len().saturating_add(size(&value));
        if cost <= self.budget {
            let mut s = self.locked();
            s.tick += 1;
            let at = s.tick;
            let entry = Entry {
                _raw: raw.clone(),
                value: Arc::clone(&value),
                cost,
                at,
            };
            // Another caller decoding the same buffer at once: the later replaces the earlier.
            if let Some(old) = s.entries.insert(id, entry) {
                s.order.remove(&old.at);
                s.used = s.used.saturating_sub(old.cost);
            }
            s.order.insert(at, id);
            s.used = s.used.saturating_add(cost);
            while s.used > self.budget {
                let Some((_, victim)) = s.order.pop_first() else {
                    break;
                };
                if let Some(gone) = s.entries.remove(&victim) {
                    s.used = s.used.saturating_sub(gone.cost);
                }
            }
        }
        Some(value)
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, State<T>> {
        // A cache, so a poisoned lock is recovered: its worst state is an entry too many.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    fn held(&self) -> usize {
        self.locked().entries.len()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn a_buffer_is_decoded_once_and_another_again() {
        let cache: ByBuffer<Vec<u8>> = ByBuffer::new(1 << 20);
        let decodes = Cell::new(0);
        let decode = |b: &Bytes| {
            decodes.set(decodes.get() + 1);
            Some(b.to_vec())
        };
        let a = Bytes::from(vec![1u8, 2, 3]);
        let one = cache.get_or(&a, Vec::len, decode).unwrap();
        let two = cache.get_or(&a.clone(), Vec::len, decode).unwrap();
        assert!(Arc::ptr_eq(&one, &two));
        assert_eq!(decodes.get(), 1);
        // The same bytes in another buffer: another entry, the same answer.
        let b = Bytes::from(vec![1u8, 2, 3]);
        let three = cache.get_or(&b, Vec::len, decode).unwrap();
        assert!(!Arc::ptr_eq(&one, &three));
        assert_eq!(*one, *three);
        assert_eq!(decodes.get(), 2);
        // A slice of `a` is another buffer: its own length.
        let four = cache.get_or(&a.slice(0..2), Vec::len, decode).unwrap();
        assert_eq!(*four, vec![1, 2]);
        assert_eq!(decodes.get(), 3);
    }

    #[test]
    fn a_refusal_is_never_kept() {
        let cache: ByBuffer<Vec<u8>> = ByBuffer::new(1 << 20);
        let a = Bytes::from(vec![9u8; 4]);
        assert!(cache.get_or(&a, Vec::len, |_| None).is_none());
        assert_eq!(cache.held(), 0);
        assert!(cache.get_or(&a, Vec::len, |b| Some(b.to_vec())).is_some());
    }

    #[test]
    fn the_budget_evicts_the_least_recently_used() {
        // Each entry costs 8: its 4-byte buffer and its 4-byte copy. Three fit in 24.
        let cache: ByBuffer<Vec<u8>> = ByBuffer::new(24);
        let bufs: Vec<Bytes> = (0..4u8).map(|i| Bytes::from(vec![i; 4])).collect();
        let decodes = Cell::new(0);
        let decode = |b: &Bytes| {
            decodes.set(decodes.get() + 1);
            Some(b.to_vec())
        };
        for b in &bufs[..3] {
            cache.get_or(b, Vec::len, decode);
        }
        // Touch the first, so the second is the oldest when the fourth arrives.
        cache.get_or(&bufs[0], Vec::len, decode);
        cache.get_or(&bufs[3], Vec::len, decode);
        assert_eq!((cache.held(), decodes.get()), (3, 4));
        cache.get_or(&bufs[0], Vec::len, decode);
        assert_eq!(decodes.get(), 4, "the first was kept");
        cache.get_or(&bufs[1], Vec::len, decode);
        assert_eq!(decodes.get(), 5, "the second was evicted");
        // Larger than the budget alone: decoded, answered, never kept.
        let big = Bytes::from(vec![0u8; 20]);
        assert!(cache.get_or(&big, Vec::len, decode).is_some());
        assert!(cache.held() <= 3);
    }
}
