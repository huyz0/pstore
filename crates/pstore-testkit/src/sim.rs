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

#[cfg(test)]
mod tests {
    use super::*;

    /// SplitMix64's first eight outputs from state 0. The first two are the published ones
    /// M8c pins; all eight come from an independent Python model of the published algorithm,
    /// not from this code. Determinism tests cannot see a mutated mixer -- it is still
    /// deterministic -- so the stream itself is pinned, on all 64 bits.
    const FROM_ZERO: [u64; 8] = [
        0xE220_A839_7B1D_CDAF,
        0x6E78_9E6A_A1B9_65F4,
        0x06C4_5D18_8009_454F,
        0xF88B_B8A8_724C_81EC,
        0x1B39_896A_51A8_749B,
        0x53CB_9F0C_747E_A2EA,
        0x2C82_9ABE_1F45_32E1,
        0xC584_133A_C916_AB3C,
    ];

    #[test]
    fn the_generator_is_splitmix64_on_all_64_bits() {
        let mut s = Sim::new(0);
        let got: Vec<u64> = (0..FROM_ZERO.len()).map(|_| s.next()).collect();
        assert_eq!(got, FROM_ZERO);
    }

    #[test]
    fn the_seed_printed_with_a_failure_is_the_one_that_replays_it() {
        // Neither 0 nor 1, which is what a constant would return.
        let mut s = Sim::new(0xDEAD_BEEF);
        s.next();
        assert_eq!(s.seed(), 0xDEAD_BEEF, "the seed must survive the draws");
    }

    #[test]
    fn steps_counts_every_decision() {
        let mut s = Sim::new(0);
        assert_eq!(s.steps(), 0);
        s.next();
        s.choose(5);
        s.chance(0.5);
        assert_eq!(s.steps(), 3);
    }

    #[test]
    fn chance_fires_below_p_and_not_at_it() {
        // The draw is the top 53 bits of the first output, as a fraction of 2^53.
        let r = (FROM_ZERO[0] >> 11) as f64 / (1u64 << 53) as f64;
        // A fresh `Sim` per call, so both see the same first draw.
        assert!(!Sim::new(0).chance(r), "p equal to the draw must not fire");
        assert!(
            Sim::new(0).chance(r.next_up()),
            "p just above the draw must fire"
        );
    }
}
