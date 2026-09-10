//! A token bucket that is *told* the time rather than reading it.

use std::time::Duration;

/// How fast a bucket refills and how much it may hold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    /// Tokens added per second.
    pub per_sec: f64,
    /// The most the bucket may hold, which is the largest instantaneous burst.
    pub burst: f64,
}

impl Rate {
    /// A rate that admits everything, for a resource a caller does not want to bound.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            per_sec: f64::INFINITY,
            burst: f64::INFINITY,
        }
    }
}

/// A token bucket.
///
/// ⚠️ **The balance may go negative, and that is the whole of the byte quota's bound.**
/// Bytes are debited *after* a transfer, because their size is unknowable before it — so a
/// single operation can overrun. If the balance saturated at zero the overrun would be
/// forgiven, a tenant could repeat "one giant read per refill tick" forever, and the bucket
/// would bound nothing over time. Carrying the debt is what turns "admits one overrun" into a
/// statement about the long run.
#[derive(Debug, Clone, Copy)]
pub struct Bucket {
    rate: Rate,
    tokens: f64,
    /// Nanoseconds since the meter's epoch, as last observed.
    at: u128,
}

impl Bucket {
    /// A full bucket at time zero.
    #[must_use]
    pub fn new(rate: Rate) -> Self {
        Self {
            rate,
            tokens: rate.burst,
            at: 0,
        }
    }

    /// Refills for the time elapsed since the last observation.
    ///
    /// ⚠️ **`now` is a parameter, not `Instant::now()`.** A bucket that read the clock would
    /// make every test's own duration an input — the test would pass on a fast machine and
    /// flake on a loaded one, and "refills at the configured rate" would be unassertable.
    /// Time also cannot go backwards here: a `now` behind the last observation refills by
    /// nothing rather than draining.
    fn refill(&mut self, now: Duration) {
        // ⚠️ An unlimited bucket is always full; a bucket at the same instant is unchanged.
        // Conflating the two -- "no time passed, so reset to full" -- makes every take free,
        // which is a quota that never refuses and passes any test with a moving clock.
        if !self.rate.per_sec.is_finite() {
            self.tokens = self.rate.burst;
            return;
        }
        let now = now.as_nanos();
        let elapsed = now.saturating_sub(self.at);
        self.at = self.at.max(now);
        if elapsed == 0 {
            return;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "nanoseconds since a test's epoch; f64 is exact past any plausible run"
        )]
        let secs = elapsed as f64 / 1e9;
        self.tokens = (self.tokens + secs * self.rate.per_sec).min(self.rate.burst);
    }

    /// Whether `n` tokens are available at `now`, taking them if so.
    ///
    /// ⚠️ **All or nothing.** A partial take would let a fan-out issue the requests it could
    /// afford and refuse the rest, which is the failure the whole-operation reservation
    /// exists to prevent.
    pub fn take(&mut self, n: f64, now: Duration) -> bool {
        self.refill(now);
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    /// Whether the balance is positive at `now`, without taking anything.
    ///
    /// The byte bucket's admission test: it cannot know what a read will move, so it asks
    /// only whether the tenant is currently in credit.
    pub fn in_credit(&mut self, now: Duration) -> bool {
        self.refill(now);
        self.tokens > 0.0
    }

    /// Debits `n`, **allowing the balance to go negative**. See the type's docs.
    pub fn debit(&mut self, n: f64, now: Duration) {
        self.refill(now);
        self.tokens -= n;
    }

    /// The current balance, for a test and for a `meta` field that reports cost.
    #[must_use]
    pub fn balance(&self) -> f64 {
        self.tokens
    }
}
