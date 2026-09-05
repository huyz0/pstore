//! Deterministic scheduling.
//!
//! A concurrency bug found once and never again is not fixed, it is forgotten. Every
//! decision here — which writer moves next, whether a writer stalls, whether the store
//! refuses — comes from a seed, so a failing run replays exactly and a fix can be shown to
//! address *that* interleaving rather than to perturb it.
//!
//! ⚠️ **A simulator is not a kernel.** It models pauses and contention; it does not
//! reproduce a scheduler, a network, or a machine losing power. Results say so.

/// A seeded source of scheduling decisions.
#[derive(Debug, Clone)]
pub struct Sim {
    state: u64,
    seed: u64,
    steps: u64,
}

impl Sim {
    /// A run driven by `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed,
            seed,
            steps: 0,
        }
    }

    /// The seed, to print with a failure so it can be replayed.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Decisions taken so far. Two runs of the same scenario must agree on this, or
    /// something is reading the clock.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// SplitMix64, written out so determinism is visible in the source rather than
    /// dependent on a crate's version.
    fn next(&mut self) -> u64 {
        self.steps += 1;
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// One of `n`, uniformly.
    ///
    /// Returns 0 for `n == 0` rather than dividing by zero: an empty choice is a caller
    /// bug, and panicking inside the scheduler would lose the seed that reproduces it.
    pub fn choose(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next() % n as u64) as usize
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        ((self.next() >> 11) as f64) / ((1u64 << 53) as f64) < p
    }

    /// A deterministic shuffle, for turning a set of pending actions into an order.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.choose(i + 1);
            items.swap(i, j);
        }
    }
}
