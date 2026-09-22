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
    /// Shortest delay before an operation is dispatched. Zero disables the delay entirely.
    ///
    /// ⚠️ **A real `tokio::time::sleep`, drawn from a stream of its own.** Two consequences,
    /// and both are the point:
    ///
    /// * Under `#[tokio::test(start_paused = true)]` the runtime auto-advances while idle, so
    ///   a 30 ms round trip costs a test nothing. Latency that made the suite slow would be
    ///   latency nobody turned on.
    /// * The draw comes from a **separate** SplitMix64 stream, so turning latency on cannot
    ///   change *which* operations fail. M0a's criterion 4 says "412, 409, 503 **and**
    ///   latency … independently", and sharing one stream would make that sentence false in
    ///   a way no functional test would notice.
    pub latency_min: std::time::Duration,
    /// Longest delay. Clamped up to `latency_min`, so an inverted pair delays by exactly
    /// `latency_min` rather than panicking or silently disabling the delay.
    pub latency_max: std::time::Duration,
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
    /// The **latency** stream, deliberately separate. Derived from `seed` so one number
    /// still reproduces a whole run, and never advanced by a fault draw so the two are
    /// independent in fact and not just in intent.
    lat_seed: u64,
    faults: Faults,
    ops: u32,
}

/// One SplitMix64 step. Shared by both streams so they cannot drift apart in behaviour.
fn split_mix(state: &mut u64) -> f64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // 53 bits of mantissa: the same value on every platform.
    #[expect(
        clippy::cast_precision_loss,
        reason = "53 bits is exactly what an f64 mantissa holds"
    )]
    {
        ((z >> 11) as f64) / ((1u64 << 53) as f64)
    }
}

impl Inner {
    fn next_f64(&mut self) -> f64 {
        split_mix(&mut self.seed)
    }

    fn next_delay(&mut self) -> std::time::Duration {
        let (lo, hi) = (self.faults.latency_min, self.faults.latency_max);
        let hi = hi.max(lo);
        if hi.is_zero() {
            return std::time::Duration::ZERO;
        }
        let r = split_mix(&mut self.lat_seed);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            clippy::cast_sign_loss,
            reason = "a span of nanoseconds scaled by a value in [0, 1)"
        )]
        let span = ((hi - lo).as_nanos() as f64 * r) as u64;
        lo + std::time::Duration::from_nanos(span)
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
                // A fixed, arbitrary decorrelation constant. One `seed` still reproduces the
                // whole run; the two streams simply never advance each other.
                lat_seed: seed ^ 0x2545_F491_4F6C_DD1D,
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

    /// Waits the operation's injected delay, if any.
    ///
    /// ⚠️ Drawn and awaited **before** the fault roll, so a failed operation costs its
    /// latency too — a backend that refuses instantly is a backend whose timeouts can never
    /// fire, and the retry paths this store exists to exercise are timeout paths.
    async fn delay(&self) {
        let d = self.lock().next_delay();
        if !d.is_zero() {
            tokio::time::sleep(d).await;
        }
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
        self.delay().await;
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.get(key).await,
        }
    }

    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.delay().await;
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.get_range(key, range).await,
        }
    }

    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, pstore_types::CasTag), BlobError> {
        self.delay().await;
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.get_with_tag(key).await,
        }
    }

    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.delay().await;
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.get_suffix(key, n).await,
        }
    }

    async fn get_tag(&self, key: &Key) -> Result<Option<pstore_types::CasTag>, BlobError> {
        // ⚠️ **The reason row 21 was a trait change and not a refactor.** This forwarded
        // cleanly while every other read consulted `read_fault`, because there was nowhere
        // to put the error — so the one read in the commit loop was the one read faults
        // could not reach, and M0c's refusal axis could only ever cover 412 and 409.
        self.delay().await;
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.get_tag(key).await,
        }
    }

    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        self.delay().await;
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.head(key).await,
        }
    }

    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        self.delay().await;
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
        self.delay().await;
        // ⚠️ The fault is decided BEFORE the backend is touched. Failing after mutating
        // would make the store diverge from what the caller was told, and every later
        // assertion would be against a fiction.
        match self.cas_fault() {
            Some(e) => Err(e),
            None => self.inner.put_conditional(key, body, pre).await,
        }
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.delay().await;
        match self.write_fault() {
            Some(e) => Err(e),
            None => self.inner.delete_batch(keys).await,
        }
    }

    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.delay().await;
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.list_unrestricted(prefix).await,
        }
    }

    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: crate::Class,
    ) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        self.delay().await;
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.get_range_as(key, range, class).await,
        }
    }

    async fn get_suffix_as(
        &self,
        key: &Key,
        n: u64,
        class: crate::Class,
    ) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        self.delay().await;
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.get_suffix_as(key, n, class).await,
        }
    }

    async fn get_immutable(&self, key: &Key, class: crate::Class) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        self.delay().await;
        match self.read_fault() {
            Some(e) => Err(e),
            None => self.inner.get_immutable(key, class).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    /// The unit-interval value `split_mix` derives from one raw 64-bit output.
    #[expect(
        clippy::cast_precision_loss,
        reason = "53 bits is exactly what an f64 mantissa holds"
    )]
    fn unit(z: u64) -> f64 {
        ((z >> 11) as f64) / ((1u64 << 53) as f64)
    }

    /// The draw the first operation on a fresh `Faulty` with `seed` will roll.
    fn first_draw(seed: u64) -> f64 {
        split_mix(&mut seed.clone())
    }

    fn faulty(seed: u64, faults: Faults) -> Faulty<MemoryStore> {
        Faulty::new(MemoryStore::new(), seed, faults)
    }

    const SEED: u64 = 7;

    /// ⚠️ SplitMix64's **published** outputs from state 0, computed independently of this
    /// file. The generator is written out so that determinism "cannot drift"; this is what
    /// makes that sentence checkable. A change to any constant or shift -- or `^=` becoming
    /// `|=`, which the nightly sweep found nothing noticed -- changes every seeded run.
    #[test]
    fn split_mix_matches_the_reference_sequence() {
        let mut state = 0u64;
        assert_eq!(
            split_mix(&mut state).to_bits(),
            unit(0xE220_A839_7B1D_CDAF).to_bits()
        );
        assert_eq!(
            split_mix(&mut state).to_bits(),
            unit(0x6E78_9E6A_A1B9_65F4).to_bits()
        );
    }

    /// ⚠️ **The premise of an exclusion, under test.** `.cargo/mutants.toml` excludes the
    /// mutant that replaces `Faults::none` with `Default::default()`, because the two are the
    /// same function. The exclusion matches the mutant's *name*, so if `none()` ever stopped
    /// being the default it would silently hide a mutant a test could kill -- and this is the
    /// assertion that fails instead.
    #[test]
    fn no_faults_is_exactly_the_default() {
        assert_eq!(Faults::none(), Faults::default());
    }

    /// ⚠️ **A rate `p` fires on `[0, p)`.** So a draw exactly equal to the rate does not fire,
    /// and a rate of 0.0 never fires even on a draw of 0.0. A seeded draw essentially never
    /// lands on a rate, which is why six `<` -> `<=` flips survived every existing test; this
    /// puts the draw on the rate deliberately. Each case also sets the rate one float above
    /// the draw, which must fire -- so a comparison that never fires fails here too.
    #[test]
    fn a_draw_equal_to_the_rate_does_not_fire_and_one_below_it_does() {
        let r = first_draw(SEED);
        let above = r.next_up();
        assert!(r > 0.0, "seed {SEED} draws exactly zero; choose another");

        let slow = |rate| Faults {
            slow_down: rate,
            ..Faults::none()
        };
        let read = |rate| Faults {
            read_error: rate,
            ..Faults::none()
        };
        let write = |rate| Faults {
            write_error: rate,
            ..Faults::none()
        };
        let lost = |rate| Faults {
            cas_lost: rate,
            ..Faults::none()
        };
        let contended = |rate| Faults {
            cas_contended: rate,
            ..Faults::none()
        };

        // read_fault: `slow_down`, then `slow_down + read_error`.
        assert!(faulty(SEED, slow(r)).read_fault().is_none());
        assert!(matches!(
            faulty(SEED, slow(above)).read_fault(),
            Some(BlobError::SlowDown)
        ));
        assert!(faulty(SEED, read(r)).read_fault().is_none());
        assert!(matches!(
            faulty(SEED, read(above)).read_fault(),
            Some(BlobError::Other(_))
        ));

        // write_fault: `slow_down`, then `slow_down + write_error`.
        assert!(faulty(SEED, slow(r)).write_fault().is_none());
        assert!(matches!(
            faulty(SEED, slow(above)).write_fault(),
            Some(BlobError::SlowDown)
        ));
        assert!(faulty(SEED, write(r)).write_fault().is_none());
        assert!(matches!(
            faulty(SEED, write(above)).write_fault(),
            Some(BlobError::Other(_))
        ));

        // cas_fault: `cas_lost`, then `cas_lost + cas_contended`.
        assert!(faulty(SEED, lost(r)).cas_fault().is_none());
        assert!(matches!(
            faulty(SEED, lost(above)).cas_fault(),
            Some(CasError::Lost)
        ));
        assert!(faulty(SEED, contended(r)).cas_fault().is_none());
        assert!(matches!(
            faulty(SEED, contended(above)).cas_fault(),
            Some(CasError::Contended)
        ));
    }
}
