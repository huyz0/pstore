//! A backend that misbehaves on purpose, reproducibly.
//!
//! Promoted from testing aid to **primary correctness vehicle** (D-99): with no cloud
//! accounts and no emulator implementing `If-None-Match: *` faithfully, this is the only
//! backend whose semantics we both control and can assert. It is also the only place
//! `409 Contended` can be produced at all — no emulator emits it.

use crate::{BlobError, Capabilities, CasError, Key, Precondition, PutOutcome};
use bytes::Bytes;
use std::sync::{Arc, Mutex};

/// Injection rates, each in `0.0..=1.0`, applied independently.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Faults {
    /// Fail reads with [`BlobError::Other`].
    pub read_error: f64,
    /// Fail unconditional writes and deletes with [`BlobError::Other`].
    ///
    /// Separate from `read_error` because a "read fault" that fails a `put` makes every
    /// test that writes a fixture flaky for the wrong reason — found exactly that way.
    pub write_error: f64,
    /// Answer with `503 SlowDown`. A normal signal, not an error.
    pub slow_down: f64,
    /// Refuse conditional writes with `412` — another writer won.
    pub cas_lost: f64,
    /// Refuse conditional writes with `409` — the condition could not be evaluated.
    pub cas_contended: f64,
    /// Deterministically `503` the first *n* operations, then stop. For testing that a
    /// transient stressor is survived rather than that a permanent one terminates.
    pub slow_down_first_n: u32,
}

impl Faults {
    /// A perfectly behaved backend. The decorator must be transparent at this setting,
    /// or every test that uses it is testing the decorator instead of the subject.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }
}

#[derive(Debug)]
struct Inner {
    /// SplitMix64. Written out rather than pulled in so that determinism is visible in
    /// the source and cannot drift with a dependency's version.
    seed: u64,
    faults: Faults,
    ops: u32,
}

impl Inner {
    fn next_f64(&mut self) -> f64 {
        self.seed = self.seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // 53 bits of mantissa: the same value on every platform.
        ((z >> 11) as f64) / ((1u64 << 53) as f64)
    }
}

/// Wraps a backend and injects faults from a seed.
#[derive(Debug, Clone)]
pub struct Faulty<S> {
    inner: Arc<S>,
    state: Arc<Mutex<Inner>>,
}

impl<S: crate::BlobStore> Faulty<S> {
    /// Wraps `inner`, injecting `faults` from `seed`.
    pub fn new(inner: S, seed: u64, faults: Faults) -> Self {
        Self {
            inner: Arc::new(inner),
            state: Arc::new(Mutex::new(Inner {
                seed,
                faults,
                ops: 0,
            })),
        }
    }

    /// Changes the injection rates, so a test can make a transient fault stop.
    pub fn set_faults(&self, faults: Faults) {
        self.lock().faults = faults;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Draws once per operation, so the sequence depends only on the seed and the number
    /// of operations — never on timing.
    fn roll(&self) -> (Faults, f64, u32) {
        let mut st = self.lock();
        st.ops = st.ops.saturating_add(1);
        let (f, ops) = (st.faults, st.ops);
        (f, st.next_f64(), ops)
    }

    fn read_fault(&self) -> Option<BlobError> {
        let (f, r, ops) = self.roll();
        if ops <= f.slow_down_first_n {
            return Some(BlobError::SlowDown);
        }
        if r < f.slow_down {
            return Some(BlobError::SlowDown);
        }
        if r < f.slow_down + f.read_error {
            return Some(BlobError::Other("injected read fault".to_owned()));
        }
        None
    }

    fn write_fault(&self) -> Option<BlobError> {
        let (f, r, ops) = self.roll();
        if ops <= f.slow_down_first_n || r < f.slow_down {
            return Some(BlobError::SlowDown);
        }
        if r < f.slow_down + f.write_error {
            return Some(BlobError::Other("injected write fault".to_owned()));
        }
        None
    }

    fn cas_fault(&self) -> Option<CasError> {
        let (f, r, _) = self.roll();
        if r < f.cas_lost {
            return Some(CasError::Lost);
        }
        if r < f.cas_lost + f.cas_contended {
            return Some(CasError::Contended);
        }
        None
    }
}

#[async_trait::async_trait]
impl<S: crate::BlobStore> crate::BlobStore for Faulty<S> {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }

    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.get(key).await,
        }
    }

    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.get_range(key, range).await,
        }
    }

    async fn get_tag(&self, key: &Key) -> Option<pstore_types::CasTag> {
        self.inner.get_tag(key).await
    }

    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.head(key).await,
        }
    }

    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        match self.write_fault() {
            Some(e) => Err(e),
            None => self.inner.put(key, body).await,
        }
    }

    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        // ⚠️ The fault is decided BEFORE the backend is touched. Failing after mutating
        // would make the store diverge from what the caller was told, and every later
        // assertion would be against a fiction.
        match self.cas_fault() {
            Some(e) => Err(e),
            None => self.inner.put_conditional(key, body, pre).await,
        }
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        match self.write_fault() {
            Some(e) => Err(e),
            None => self.inner.delete_batch(keys).await,
        }
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.list_unrestricted(prefix).await,
        }
    }
}
