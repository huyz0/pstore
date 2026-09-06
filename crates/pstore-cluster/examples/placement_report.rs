//! The placement numbers M4a is pinned to, as a runnable measurement.
//!
//! ⚠️ Exists because the spec's thresholds were **measured before being written**, and a
//! threshold nobody can reproduce is a threshold nobody can check. It also records the one
//! that was wrong: "churn ≤1.6× the minimum" for a single node added was one sample of a
//! statistic that spans 1.47–2.10× depending on which node id is added.
//!
//!   cargo run --release -p pstore-cluster --example placement_report
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "a measurement harness: an index out of range is a bug in the harness"
)]

use pstore_cluster::{Placement, Roster};
use std::collections::{BTreeMap, BTreeSet};

const R: usize = 3;
const KEYS: usize = 100_000;

fn roster(n: usize) -> Roster {
    Roster::from_nodes((0..n).map(|i| format!("n{i}")))
}

fn keys() -> Vec<String> {
    (0..KEYS).map(|i| format!("idx{i}/s{}", i % 7)).collect()
}

fn main() {
    let ks = keys();
    println!("# {KEYS} keys, R={R}, C=32. Provisional: a simulation of a pure function.\n");

    println!("## Balance — max node load as a multiple of the mean");
    for n in [50usize, 100, 500, 2000] {
        let r = roster(n);
        let p = Placement::new(&r);
        let mut load: BTreeMap<&str, usize> = BTreeMap::new();
        for k in &ks {
            for id in p.place(k, R) {
                *load.entry(id).or_default() += 1;
            }
        }
        let mean = (KEYS * R) as f64 / n as f64;
        let (max, min) = (
            *load.values().max().unwrap() as f64,
            *load.values().min().unwrap() as f64,
        );
        println!(
            "  N={n:<4} max/mean {:.3}   min/mean {:.3}   nodes with nothing: {}",
            max / mean,
            min / mean,
            n - load.len()
        );
    }
    println!(
        "  ⚠️ C grows as 3·√N. With C FIXED at 32 this degrades badly with fleet size —\n\
         \x20    measured 1.208 at N=100 and 1.811 at N=2000 — which D-5's \"C ≈ 32\" does not\n\
         \x20    say. Neither setting reproduces its \"within 10–15% of average\"."
    );

    println!("\n## Churn — replica slots reassigned, against the floor the change forces");
    for (a, b) in [(100usize, 150usize), (150, 100), (100, 200)] {
        let (ra, rb) = (roster(a), roster(b));
        let (pa, pb) = (Placement::new(&ra), Placement::new(&rb));
        let moved: usize = ks
            .iter()
            .map(|k| {
                let x: BTreeSet<_> = pa.place(k, R).into_iter().collect();
                let y: BTreeSet<_> = pb.place(k, R).into_iter().collect();
                x.difference(&y).count()
            })
            .sum();
        let frac = moved as f64 / (KEYS * R) as f64;
        let floor = a.abs_diff(b) as f64 / a.max(b) as f64;
        println!(
            "  {a:>4} -> {b:<4} moved {:5.1}%   floor {:5.1}%   {:.2}x",
            frac * 100.0,
            floor * 100.0,
            frac / floor
        );
    }

    println!("\n## Churn — one node added, over 20 different node ids");
    let base: Vec<String> = (0..100).map(|i| format!("n{i}")).collect();
    let ra = Roster::from_nodes(base.clone());
    let pa = Placement::new(&ra);
    let mut ratios: Vec<f64> = Vec::new();
    for t in 0..20 {
        let rb = Roster::from_nodes(base.iter().cloned().chain([format!("extra{t}")]));
        let pb = Placement::new(&rb);
        let moved: usize = ks
            .iter()
            .map(|k| {
                let x: BTreeSet<_> = pa.place(k, R).into_iter().collect();
                let y: BTreeSet<_> = pb.place(k, R).into_iter().collect();
                x.difference(&y).count()
            })
            .sum();
        ratios.push(moved as f64 / (KEYS * R) as f64);
    }
    ratios.sort_by(f64::total_cmp);
    let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
    println!(
        "  moved  min {:.2}%  mean {:.2}%  max {:.2}%   (floor {:.2}%)",
        ratios[0] * 100.0,
        mean * 100.0,
        ratios[19] * 100.0,
        100.0 / 101.0
    );
    println!(
        "  ⚠️ As a RATIO to that floor this spans {:.2}x–{:.2}x. The spec first pinned 1.6x\n\
         \x20    from a single sample; a ratio to a 1% denominator measures ring-position luck.\n\
         \x20    The absolute figure is what is stable, and what the criterion now bounds.",
        ratios[0] / (1.0 / 101.0),
        ratios[19] / (1.0 / 101.0)
    );

    println!("\n## Nothing is copied");
    println!(
        "  A fleet change issues 0 blob requests: placement is a pure function of the roster.\n\
         \x20 That is what M4's \"add/remove 50% of the fleet with zero data movement\" means —\n\
         \x20 churn above is large and bytes moved between nodes is zero."
    );
}
