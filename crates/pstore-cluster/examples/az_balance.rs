//! OQ-59: what does constraining placement to an availability zone cost in balance?
//!
//! ⚠️ **The question had to be split before it could be answered.** A first attempt compared
//! one 900-node ring against three 300-node rings and reported the difference as the cost of
//! AZ-awareness. Review measured the effect at 0.026 on the mean against a spread of 0.28
//! across node-naming trials — an order of magnitude smaller than the noise — and pointed out
//! that it moves two variables at once: the AZ constraint *and* per-ring `N`, and therefore
//! `window()`, which pulls the other way.
//!
//! So this measures imbalance against `N` with enough trials to see past the spread. The
//! answer is in two parts, and the second is the whole of it:
//!
//! * at a fixed ring size, constraining to an AZ costs **nothing** — there is no AZ term in
//!   `place`, and a 300-node cell is a 300-node ring;
//! * and splitting a fleet into smaller rings costs **nothing measurable either** — which is
//!   the opposite of what this milestone's spec assumed before it was measured.
//!
//! ⚠️ `provisional`: measured on WSL2.
#![allow(clippy::print_stdout, reason = "a reporting example is its own output")]

use pstore_cluster::{Placement, Roster};
use std::collections::HashMap;

const KEYS: usize = 100_000;
const R: usize = 3;
const TRIALS: u32 = 8;

fn imbalance(n: usize, trial: u32) -> f64 {
    // The trial index varies the node NAMES, which is what moves ring positions. Varying only
    // the key set would re-measure one ring eight times and report its spread as zero.
    let roster =
        Roster::from_nodes((0..n).map(|i| format!("10.{trial}.{}.{}:7946", i / 256, i % 256)));
    let p = Placement::new(&roster);
    let mut load: HashMap<&str, usize> = HashMap::new();
    for i in 0..KEYS {
        for node in p.place(&format!("t/idx{i}/s0"), R) {
            *load.entry(node).or_default() += 1;
        }
    }
    let mean = (KEYS * R) as f64 / n as f64;
    load.values().copied().max().unwrap_or(0) as f64 / mean
}

fn main() {
    println!("OQ-59 — imbalance (max node load / mean), {KEYS} keys, R={R}, {TRIALS} trials");
    println!("⚠️ provisional: measured on WSL2\n");
    println!("{:>7} {:>9} {:>9} {:>9}", "nodes", "min", "mean", "max");
    for n in [100usize, 300, 900] {
        let samples: Vec<f64> = (0..TRIALS).map(|t| imbalance(n, t)).collect();
        let mean = samples.iter().sum::<f64>() / f64::from(TRIALS);
        let lo = samples.iter().copied().fold(f64::MAX, f64::min);
        let hi = samples.iter().copied().fold(0.0, f64::max);
        println!("{n:>7} {lo:>9.3} {mean:>9.3} {hi:>9.3}");
    }
    println!(
        "\nAnswer: AZ-aware placement costs essentially NOTHING in balance.\n\
         \n\
         At a fixed ring size the constraint is free -- `place` has no AZ term, so a 300-node\n\
         cell IS a 300-node ring. And shrinking the ring does not hurt either: the trial\n\
         spread (about +-0.1) is wider than the gap between 100 and 900 nodes.\n\
         \n\
         The mechanism runs OPPOSITE to the intuition this milestone was specified on.\n\
         `window(n) = max(32, 3*sqrt(n))` covers ~32% of a 100-node ring but only ~10% of a\n\
         900-node one, and a relatively wider window balances better -- which offsets the law\n\
         of large numbers rather than compounding with it."
    );
}
