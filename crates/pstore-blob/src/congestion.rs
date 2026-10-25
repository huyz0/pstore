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
    active: AtomicU32,
    available: tokio::sync::Notify,
}

struct Admission<'a>(&'a State);

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
        self.0.available.notify_waiters();
    }
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
        let ceiling = ceiling.max(1);
        Self {
            inner: Arc::new(inner),
            state: Arc::new(State {
                limit: AtomicU32::new(ceiling),
                ceiling,
                streak: AtomicU32::new(0),
                attempts: AtomicU64::new(0),
                active: AtomicU32::new(0),
                available: tokio::sync::Notify::new(),
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
        let _ = self
            .state
            .limit
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some((cur / 2).max(1))
            });
    }

    /// Additive increase, and only every few successes, so one lucky request does not
    /// undo a cut.
    fn on_success(&self) {
        let n = self
            .state
            .streak
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        if n.is_multiple_of(8) {
            let _ = self
                .state
                .limit
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                    Some(cur.saturating_add(1).min(self.state.ceiling))
                });
            self.state.available.notify_waiters();
        }
    }

    async fn admit(&self) -> Admission<'_> {
        loop {
            let notified = self.state.available.notified();
            tokio::pin!(notified);
            // Register before checking occupancy, so a concurrent release cannot be lost.
            notified.as_mut().enable();
            let active = self.state.active.load(Ordering::Acquire);
            if active < self.limit()
                && self
                    .state
                    .active
                    .compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                return Admission(&self.state);
            }
            if self.state.active.load(Ordering::Acquire) < self.limit() {
                continue;
            }
            notified.await;
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
            let admission = self.admit().await;
            self.state.attempts.fetch_add(1, Ordering::Relaxed);
            let outcome = op().await;
            drop(admission);
            match outcome {
                Ok(v) => {
                    self.on_success();
                    return Ok(v);
                }
                Err(BlobError::SlowDown) => {
                    self.on_slow_down();
                    last = BlobError::SlowDown;
                    tokio::time::sleep(retry_delay(attempt)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }
}

/// How long to wait before retrying a throttled request.
///
/// ⚠️ Split out from the retry loop so it can be checked. Inside the loop its only
/// observable was elapsed time, and mutation testing inverted the shift — turning
/// exponential backoff into an immediately-collapsing one — without a single test
/// noticing. A fleet that all tripped together must not all come back together.
#[must_use]
pub fn retry_delay(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(1u64 << attempt.min(16))
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

    async fn get_tag(&self, key: &Key) -> Result<Option<pstore_types::CasTag>, BlobError> {
        // ⚠️ Retried like every other read, and it is the newest of them: this forwarded
        // directly until `get_tag` became fallible, because a method returning `Option` has
        // nothing for `with_retry` to inspect. It is the rebase step of the commit protocol,
        // so a 503 here abandoned a commit that a single backoff would have landed -- and
        // `on_slow_down` was never reached, leaving the limit at full concurrency against a
        // prefix that is shedding. Backlog row 23.
        self.with_retry(|| self.inner.get_tag(key)).await
    }

    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.with_retry(|| self.inner.head(key)).await
    }

    async fn stat(&self, key: &Key) -> Result<(u64, Option<pstore_types::CasTag>), BlobError> {
        self.with_retry(|| self.inner.stat(key)).await
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
        let _admission = self.admit().await;
        self.inner.put_conditional(key, body, pre).await
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.with_retry(|| self.inner.delete_batch(keys)).await
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.with_retry(|| self.inner.list_unrestricted(prefix))
            .await
    }

    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: crate::Class,
    ) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        self.with_retry(|| self.inner.get_range_as(key, range.clone(), class))
            .await
    }

    async fn get_suffix_as(
        &self,
        key: &Key,
        n: u64,
        class: crate::Class,
    ) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        self.with_retry(|| self.inner.get_suffix_as(key, n, class))
            .await
    }

    async fn get_immutable(&self, key: &Key, class: crate::Class) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        self.with_retry(|| self.inner.get_immutable(key, class))
            .await
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use std::task::{Context, Poll, Waker};

    #[tokio::test]
    async fn concurrency_admission_blocks_and_releases_on_cancellation() {
        let store = Congested::new(crate::MemoryStore::new(), 1);
        let mut cx = Context::from_waker(Waker::noop());
        let mut first = Box::pin(store.with_retry(std::future::pending::<Result<(), BlobError>>));
        assert!(first.as_mut().poll(&mut cx).is_pending());
        let mut second = Box::pin(store.with_retry(|| async { Ok(7) }));
        assert!(
            second.as_mut().poll(&mut cx).is_pending(),
            "backend concurrency exceeded the ceiling"
        );
        drop(first);
        assert!(matches!(second.as_mut().poll(&mut cx), Poll::Ready(Ok(7))));
    }

    #[tokio::test]
    async fn a_reduced_limit_blocks_new_admissions() {
        let store = Congested::new(crate::MemoryStore::new(), 2);
        let mut cx = Context::from_waker(Waker::noop());
        let mut first = Box::pin(store.with_retry(std::future::pending::<Result<(), BlobError>>));
        let mut second = Box::pin(store.with_retry(std::future::pending::<Result<(), BlobError>>));
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        store.on_slow_down();
        drop(first);
        let mut third = Box::pin(store.with_retry(|| async { Ok(9) }));
        assert!(
            third.as_mut().poll(&mut cx).is_pending(),
            "reduced limit admitted a request while full"
        );
        drop(second);
        assert!(matches!(third.as_mut().poll(&mut cx), Poll::Ready(Ok(9))));
    }
}

#[cfg(test)]
mod admission_wakeup_tests {
    use super::*;
    use crate::BlobStore;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll, Wake, Waker};
    struct Signal(AtomicBool);
    impl Wake for Signal {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    #[tokio::test]
    async fn releasing_capacity_wakes_a_conditional_write_waiter() {
        let store = Congested::new(crate::MemoryStore::new(), 1);
        let signal = Arc::new(Signal(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&signal));
        let mut cx = Context::from_waker(&waker);
        let mut first = Box::pin(store.with_retry(std::future::pending::<Result<(), BlobError>>));
        assert!(first.as_mut().poll(&mut cx).is_pending());
        let key = Key::new("conditional");
        let mut second = Box::pin(store.put_conditional(
            &key,
            Bytes::from_static(b"x"),
            Precondition::NotExists,
        ));
        assert!(
            second.as_mut().poll(&mut cx).is_pending(),
            "conditional writes bypass admission"
        );
        signal.0.store(false, Ordering::SeqCst);
        drop(first);
        assert!(
            signal.0.load(Ordering::SeqCst),
            "capacity release did not wake the waiter"
        );
        assert!(matches!(second.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    }
}

#[cfg(test)]
mod limit_edge_tests {
    use super::*;
    #[test]
    fn zero_concurrency_still_allows_progress() {
        let store = Congested::new(crate::MemoryStore::new(), 0);
        assert_eq!(store.limit(), 1);
    }
    #[test]
    fn recovery_at_the_largest_ceiling_does_not_overflow() {
        let store = Congested::new(crate::MemoryStore::new(), u32::MAX);
        store.state.streak.store(7, Ordering::Relaxed);
        store.on_success();
        assert_eq!(store.limit(), u32::MAX);
    }
    #[test]
    fn the_success_counter_can_cross_its_integer_boundary() {
        let store = Congested::new(crate::MemoryStore::new(), 8);
        store.state.streak.store(u32::MAX, Ordering::Relaxed);
        store.on_success();
        assert_eq!(store.limit(), 8);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "benchmark setup and reads must succeed")]
mod admission_bench {
    use super::*;
    use crate::BlobStore;
    #[tokio::test]
    #[ignore = "local microbenchmark; timings are provisional"]
    async fn admission_microbench() {
        let raw = crate::MemoryStore::new();
        let key = Key::new("bench");
        raw.put(&key, Bytes::from_static(b"x")).await.unwrap();
        let limited = Congested::new(raw.clone(), 64);
        const N: u32 = 100_000;
        for sample in 0..5 {
            let start = std::time::Instant::now();
            for _ in 0..N {
                std::hint::black_box(raw.head(&key).await.unwrap());
            }
            let raw_ns = start.elapsed().as_secs_f64() * 1e9 / f64::from(N);
            let start = std::time::Instant::now();
            for _ in 0..N {
                std::hint::black_box(limited.head(&key).await.unwrap());
            }
            let limited_ns = start.elapsed().as_secs_f64() * 1e9 / f64::from(N);
            println!(
                "provisional sample={sample} iterations={N} raw_ns={raw_ns:.1} limited_ns={limited_ns:.1}"
            );
        }
    }
}
