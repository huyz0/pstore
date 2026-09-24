//! A backend that fails the way a real one fails: sometimes, and not politely.
//!
//! Distinct from [`crate::broken`], which models a backend that is *consistently wrong*.
//! This one is correct when it answers at all — it just refuses a deterministic fraction
//! of writes, which is what a throttled bucket, a torn connection, or a process dying
//! mid-`PUT` looks like from above. The failures come from a seed, so a run that finds a
//! lost write replays exactly.

use bytes::Bytes;
use pstore_blob::{
    BlobError, BlobStore, Capabilities, CasError, Key, MemoryStore, Precondition, PutOutcome,
};
use pstore_types::CasTag;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// A correct store that refuses some writes.
#[derive(Debug)]
pub struct Flaky {
    inner: MemoryStore,
    state: AtomicU64,
    /// Probability in [0, 1) that any one write is refused.
    rate: u64,
    /// Ordinals of writes to refuse outright, for a scenario that needs a *specific*
    /// failure rather than a plausible distribution of them.
    at: Vec<u64>,
    /// Ordinals of READS to refuse, for a backend that is briefly unreachable rather than
    /// down. A store that is down tests the give-up path; one that is briefly unreachable
    /// tests the retry, and those are different bugs.
    reads_fail_at: Vec<u64>,
    /// Reads refused so far, counted separately: a read gate and a write gate share nothing
    /// but the store, and a test asserting "three refusals" must not be satisfied by three
    /// of the other kind.
    reads_seen: AtomicU64,
    /// Every read fails. Models a backend that is down rather than busy.
    reads_fail: bool,
    /// `head` never reports a missing key.
    ///
    /// The one answer forward probing depends on. A backend that never gives it — hostile,
    /// buggy, or serving a corrupted lane — turns tail discovery into a loop with no exit.
    never_missing: bool,
    /// Every conditional write comes back `Contended`.
    ///
    /// Distinct from `Lost`, and the difference is the whole retry protocol: `Lost` means
    /// rebase because someone else won, `Contended` means the backend could not evaluate
    /// the condition and the same attempt should be retried unchanged.
    always_contended: bool,
    seen: AtomicU64,
    failures: Arc<AtomicU64>,
}

impl Flaky {
    /// A store refusing writes at roughly `rate`, driven by `seed`.
    #[must_use]
    pub fn new(seed: u64, rate: f64) -> Self {
        let clamped = rate.clamp(0.0, 1.0);
        Self {
            inner: MemoryStore::new(),
            state: AtomicU64::new(seed),
            // Compared against a 32-bit draw, so the rate is exact and integral rather
            // than dependent on float formatting.
            rate: (clamped * f64::from(u32::MAX)) as u64,
            at: Vec::new(),
            reads_fail: false,
            reads_fail_at: Vec::new(),
            reads_seen: AtomicU64::new(0),
            never_missing: false,
            always_contended: false,
            seen: AtomicU64::new(0),
            failures: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A store that refuses exactly the writes at these ordinals and no others.
    ///
    /// A seeded rate proves a property holds *in general*; this pins the one interleaving
    /// a regression test is about, so a failure names the bug instead of a seed.
    #[must_use]
    pub fn refusing(at: &[u64]) -> Self {
        Self {
            inner: MemoryStore::new(),
            state: AtomicU64::new(0),
            rate: 0,
            at: at.to_vec(),
            reads_fail: false,
            reads_fail_at: Vec::new(),
            reads_seen: AtomicU64::new(0),
            never_missing: false,
            always_contended: false,
            seen: AtomicU64::new(0),
            failures: Arc::new(AtomicU64::new(0)),
        }
    }

    /// How many writes have been refused, so a test can assert it actually injected.
    #[must_use]
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// SplitMix64 over an atomic, so concurrent writers still draw deterministically from
    /// one stream. The *order* they draw in is the scheduler's business, not this store's.
    fn refuse(&self) -> bool {
        let ordinal = self.seen.fetch_add(1, Ordering::Relaxed);
        if self.at.contains(&ordinal) {
            self.failures.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        if self.rate == 0 {
            return false;
        }
        let mut z = self
            .state
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let draw = (z ^ (z >> 31)) >> 32;
        if draw < self.rate {
            self.failures.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// A store whose reads all fail.
    #[must_use]
    pub fn refusing_reads() -> Self {
        // Only `reads_fail`: every other field, the read ordinals included, is what
        // `refusing(&[])` already sets, and restating them was two equivalent mutants (M8i).
        Self {
            reads_fail: true,
            ..Self::refusing(&[])
        }
    }

    /// A store whose `head` always finds the key, so a forward probe never terminates.
    #[must_use]
    pub fn never_missing() -> Self {
        Self {
            never_missing: true,
            ..Self::refusing(&[])
        }
    }

    /// A store whose conditional writes are always `Contended`.
    #[must_use]
    pub fn always_contended() -> Self {
        Self {
            always_contended: true,
            ..Self::refusing(&[])
        }
    }

    /// A store whose reads at these ordinals fail and whose others succeed.
    ///
    /// ⚠️ Models a backend that is *briefly* unreachable, which is the case a retry budget
    /// exists for. `refusing_reads` models one that is down, and a node must tell them apart:
    /// giving up on the first is how a cold fleet loses members it never had to.
    #[must_use]
    pub fn refusing_reads_at(at: &[u64]) -> Self {
        Self {
            reads_fail_at: at.to_vec(),
            ..Self::refusing(&[])
        }
    }

    fn read_gate(&self) -> Result<(), BlobError> {
        let ordinal = self.reads_seen.fetch_add(1, Ordering::Relaxed);
        if self.reads_fail_at.contains(&ordinal) {
            self.failures.fetch_add(1, Ordering::Relaxed);
            return Err(BlobError::Other("injected read failure".to_owned()));
        }
        if self.reads_fail {
            self.failures.fetch_add(1, Ordering::Relaxed);
            return Err(BlobError::Other("injected read failure".to_owned()));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl BlobStore for Flaky {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }
    async fn get(&self, key: &Key) -> Result<Bytes, BlobError> {
        self.read_gate()?;
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &Key, range: std::ops::Range<u64>) -> Result<Bytes, BlobError> {
        self.read_gate()?;
        self.inner.get_range(key, range).await
    }
    async fn get_suffix(&self, key: &Key, n: u64) -> Result<Bytes, BlobError> {
        self.read_gate()?;
        self.inner.get_suffix(key, n).await
    }
    async fn get_with_tag(&self, key: &Key) -> Result<(Bytes, CasTag), BlobError> {
        self.read_gate()?;
        self.inner.get_with_tag(key).await
    }
    async fn get_tag(&self, key: &Key) -> Result<Option<CasTag>, BlobError> {
        self.inner.get_tag(key).await
    }
    async fn head(&self, key: &Key) -> Result<u64, BlobError> {
        // ⚠️ Gated too. `head` is how the WAL tail is probed, so leaving it working while
        // every other read fails models a backend that does not exist and hides the
        // engine's most important "is this a gap or an outage?" decision.
        self.read_gate()?;
        if self.never_missing {
            return Ok(1);
        }
        self.inner.head(key).await
    }

    async fn put(&self, key: &Key, body: Bytes) -> Result<PutOutcome, BlobError> {
        if self.refuse() {
            // ⚠️ Refused BEFORE writing. A failure that half-lands is a different
            // scenario, and pretending one is the other would let a real torn-write bug
            // hide behind a passing test for this one.
            return Err(BlobError::Other("injected write failure".to_owned()));
        }
        self.inner.put(key, body).await
    }

    async fn put_conditional(
        &self,
        key: &Key,
        body: Bytes,
        pre: Precondition,
    ) -> Result<PutOutcome, CasError> {
        if self.always_contended {
            self.failures.fetch_add(1, Ordering::Relaxed);
            return Err(CasError::Contended);
        }
        if self.refuse() {
            return Err(CasError::Io(
                "injected conditional write failure".to_owned(),
            ));
        }
        self.inner.put_conditional(key, body, pre).await
    }

    async fn delete_batch(&self, keys: &[Key]) -> Result<(), BlobError> {
        self.inner.delete_batch(keys).await
    }
    async fn list_unrestricted(&self, prefix: &Key) -> Result<Vec<Key>, BlobError> {
        self.inner.list_unrestricted(prefix).await
    }

    async fn get_range_as(
        &self,
        key: &Key,
        range: std::ops::Range<u64>,
        class: pstore_blob::Class,
    ) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        self.read_gate()?;
        self.inner.get_range_as(key, range, class).await
    }

    async fn get_suffix_as(
        &self,
        key: &Key,
        n: u64,
        class: pstore_blob::Class,
    ) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        self.read_gate()?;
        self.inner.get_suffix_as(key, n, class).await
    }

    async fn get_immutable(
        &self,
        key: &Key,
        class: pstore_blob::Class,
    ) -> Result<Bytes, BlobError> {
        // ⚠️ Forwards the class. Inheriting the trait default drops it, and the read is
        // then admitted as `Bulk` -- D-21 off, with every test still green.

        self.read_gate()?;
        self.inner.get_immutable(key, class).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 32 bits `refuse` keeps (`>> 32`) of SplitMix64's first eight outputs from state 0,
    /// from an independent Python model of the published algorithm. The first two are the
    /// published `0xE220A8397B1DCDAF` and `0x6E789E6AA1B965F4`.
    const DRAWS_FROM_ZERO: [u64; 8] = [
        0xE220_A839,
        0x6E78_9E6A,
        0x06C4_5D18,
        0xF88B_B8A8,
        0x1B39_896A,
        0x53CB_9F0C,
        0x2C82_9ABE,
        0xC584_133A,
    ];

    #[test]
    fn a_rate_refuses_below_it_and_not_at_it_on_every_pinned_draw() {
        // Two stores in lockstep from one seed, so both see draw `d` on the same call: one
        // at exactly `d` must not refuse, one at `d + 1` must. That pins the edge and, over
        // eight draws, the 32 bits of each draw that escape the mixer.
        let (mut at, mut above) = (Flaky::new(0, 0.0), Flaky::new(0, 0.0));
        for (i, d) in DRAWS_FROM_ZERO.into_iter().enumerate() {
            // `rate == 0` short-circuits without drawing, so every pinned draw is non-zero.
            at.rate = d;
            above.rate = d + 1;
            assert!(
                !at.refuse(),
                "draw {i}: a rate equal to the draw {d:#x} refused"
            );
            assert!(
                above.refuse(),
                "draw {i}: a rate just above the draw {d:#x} did not"
            );
        }
    }
}
