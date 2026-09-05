//! Adaptive concurrency and bounded retry.
//!
//! `503 SlowDown` is a **normal signal**, not an error: S3 repartitions a prefix
//! reactively, and during the scale-up it sheds. Not backing off produces a 503 storm;
//! retrying forever is how a transient stressor becomes a metastable failure, which is
//! the single most common sustaining effect in the published incident studies.

use crate::{BlobError, Capabilities, CasError, Key, Precondition, PutOutcome};
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// How many attempts one operation may make before giving up.
///
/// Bounded because an unbounded retry is a load amplifier: the requests that fail are
/// exactly the ones that come back.
const MAX_ATTEMPTS: u32 = 4;

#[derive(Debug)]
struct State {
    limit: AtomicU32,
    ceiling: u32,
    /// Consecutive successes since the last cut, so recovery is gradual rather than a
    /// step back to the ceiling that would immediately re-trip.
    streak: AtomicU32,
    attempts: AtomicU64,
}

/// Wraps a backend with AIMD-style concurrency control and bounded retry.
///
/// Additive increase, multiplicative decrease — the shape TCP settled on, for the same
/// reason: a cut must be fast enough to relieve the congestion and a recovery slow enough
/// not to recreate it.
#[derive(Debug, Clone)]
pub struct Congested<S> {
    inner: Arc<S>,
    state: Arc<State>,
}

impl<S: crate::BlobStore> Congested<S> {
    /// Wraps `inner` starting at `ceiling` concurrent requests.
    pub fn new(inner: S, ceiling: u32) -> Self {
        Self {
            inner: Arc::new(inner),
            state: Arc::new(State {
                limit: AtomicU32::new(ceiling),
                ceiling,
                streak: AtomicU32::new(0),
                attempts: AtomicU64::new(0),
            }),
        }
    }

    /// The current concurrency limit.
    #[must_use]
    pub fn limit(&self) -> u32 {
        self.state.limit.load(Ordering::Relaxed)
    }

    /// Total attempts made, including retries. The number that would grow without bound
    /// if the retry policy were wrong.
    #[must_use]
    pub fn attempts(&self) -> u64 {
        self.state.attempts.load(Ordering::Relaxed)
    }

    /// Multiplicative decrease, floored at one: the limit must never reach zero, or the
    /// backend can never be probed again and the fall is permanent.
    fn on_slow_down(&self) {
        self.state.streak.store(0, Ordering::Relaxed);
        let cur = self.state.limit.load(Ordering::Relaxed);
        self.state.limit.store((cur / 2).max(1), Ordering::Relaxed);
    }

    /// Additive increase, and only every few successes, so one lucky request does not
    /// undo a cut.
    fn on_success(&self) {
        let n = self.state.streak.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(8) {
            let cur = self.state.limit.load(Ordering::Relaxed);
            self.state
                .limit
                .store((cur + 1).min(self.state.ceiling), Ordering::Relaxed);
        }
    }

    /// Runs `op`, backing off and retrying while it answers `SlowDown`.
    async fn with_retry<T, F, Fut>(&self, mut op: F) -> Result<T, BlobError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, BlobError>>,
    {
        let mut last = BlobError::SlowDown;
        for attempt in 0..MAX_ATTEMPTS {
            self.state.attempts.fetch_add(1, Ordering::Relaxed);
            match op().await {
                Ok(v) => {
                    self.on_success();
                    return Ok(v);
                }
                Err(BlobError::SlowDown) => {
                    self.on_slow_down();
                    last = BlobError::SlowDown;
                    // Exponential, and jittered by the attempt index so a fleet that all
                    // tripped together does not all come back together.
                    let ms = 1u64 << attempt;
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }
}

#[async_trait::async_trait]
impl<S: crate::BlobStore> crate::BlobStore for Congested<S> {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }

    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.with_retry(|| self.inner.get(key)).await
    }

    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.with_retry(|| self.inner.get_range(key, range.clone()))
            .await
    }

    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, pstore_types::CasTag), BlobError> {
        self.with_retry(|| self.inner.get_with_tag(key)).await
    }

    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.with_retry(|| self.inner.get_suffix(key, n)).await
    }

    async fn get_tag(&self, key: &Key) -> Option<pstore_types::CasTag> {
        self.inner.get_tag(key).await
    }

    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.with_retry(|| self.inner.head(key)).await
    }

    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.with_retry(|| self.inner.put(key, body.clone())).await
    }

    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        // ⚠️ Deliberately NOT retried here. A CAS failure is a decision for the caller:
        // `Lost` means rebase against the new state, `Contended` means retry the same
        // attempt. Retrying blindly at this layer would re-send a body built from a world
        // that no longer exists.
        self.inner.put_conditional(key, body, pre).await
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.with_retry(|| self.inner.delete_batch(keys)).await
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.with_retry(|| self.inner.list_unrestricted(prefix))
            .await
    }
}
