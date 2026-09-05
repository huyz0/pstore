//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Balanced clustering (D-8), and the two ways it fails silently.
//!
//! An unbalanced clustering still answers every query and still returns plausible results.
//! What it does is make one posting list enormous — so probing it costs a multiple of the
//! byte budget while probing its neighbours returns almost nothing, and recall and cost
//! both degrade in a way no functional test sees.
//!
//! A *lossy* clustering is worse: a vector assigned to no list is unreachable at any `p`,
//! and shows up only as recall that will not go above some ceiling however wide the probe.

use pstore_index::cluster::{Clustering, Params};

const DIM: usize = 64;

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    }
    fn normal(&mut self) -> f32 {
        (0..6).map(|_| self.unit()).sum()
    }
}

/// A mixture with deliberately uneven cluster populations.
fn skewed(n: usize, groups: usize, skew: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    let centres: Vec<Vec<f32>> = (0..groups)
        .map(|_| (0..DIM).map(|_| rng.normal() * 3.0).collect())
        .collect();
    // Group 0 gets `skew` times the share of every other group, which is what a real
    // corpus looks like: a few dense topics and a long tail.
    let weights: Vec<usize> = (0..groups).map(|g| if g == 0 { skew } else { 1 }).collect();
    let total: usize = weights.iter().sum();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut pick = (i * total / n) % total;
        let mut g = 0;
        for (gi, w) in weights.iter().enumerate() {
            if pick < *w {
                g = gi;
                break;
            }
            pick -= w;
        }
        out.push(
            centres[g]
                .iter()
                .map(|c| c + rng.normal() * 0.4)
                .collect::<Vec<f32>>(),
        );
    }
    out
}

/// The same mixture, but with rows GROUPED by cluster rather than interleaved.
///
/// This is what ingestion actually looks like: documents arrive in topic order, a crawl at
/// a time, not shuffled.
fn grouped(n: usize, groups: usize, skew: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut v = skewed(n, groups, skew, seed);
    // Stable sort by which centre each vector is nearest to -- cheap stand-in for arrival
    // order, and deterministic.
    v.sort_by(|a, b| a[0].total_cmp(&b[0]));
    v
}

fn uniform(n: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    (0..n)
        .map(|_| (0..DIM).map(|_| rng.normal()).collect())
        .collect()
}

#[test]
fn every_vector_is_reachable_from_at_least_one_list() {
    // ⚠️ The lossy failure. A dropped vector is invisible to every functional test and
    // shows up only as a recall ceiling no probe width can lift.
    for (name, corpus) in [
        ("uniform", uniform(2_000, 1)),
        ("skewed", skewed(2_000, 8, 10, 2)),
    ] {
        let c = Clustering::build(&corpus, Params::default());
        let mut seen = vec![false; corpus.len()];
        for list in c.lists() {
            for row in list {
                seen[*row] = true;
            }
        }
        let lost = seen.iter().filter(|x| !**x).count();
        assert_eq!(lost, 0, "{name}: {lost} vectors are in no posting list");
    }
}

#[test]
fn clustering_stays_balanced_on_skewed_data() {
    // ⚠️ 10:1 density skew, which is the case naive k-means collapses on: centroids chase
    // the dense region, the sparse tail ends up in one enormous list, and probing it costs
    // a multiple of the byte budget.
    // 6,000 vectors into lists of 500 => 12 lists, which exercises balance harder than
    // 20,000 into 5 and runs in a fifth of the time. A suite `cargo mutants` re-runs once
    // per mutant cannot afford fixtures sized by habit.
    let corpus = skewed(6_000, 10, 10, 3);
    let c = Clustering::build(
        &corpus,
        Params {
            target_list_size: 500,
            ..Params::default()
        },
    );
    let sizes: Vec<usize> = c.lists().iter().map(Vec::len).collect();
    let mean = corpus.len() as f64 / sizes.len() as f64;
    let largest = *sizes.iter().max().unwrap();
    assert!(
        (largest as f64) <= mean * 4.0,
        "largest posting list has {largest} vectors against a mean of {mean:.0}: over the \
         4x bound, so one probe costs {:.1}x its share of the byte budget",
        largest as f64 / mean
    );
    assert!(
        sizes.iter().all(|s| *s > 0),
        "an empty posting list was kept"
    );
}

#[test]
fn the_list_count_follows_the_target_size() {
    // Lists exist to be one useful ranged GET. Too many and a probe fetches nothing useful;
    // too few and it fetches the segment.
    let corpus = uniform(4_000, 4);
    let c = Clustering::build(
        &corpus,
        Params {
            target_list_size: 500,
            ..Params::default()
        },
    );
    assert!(
        (6..=10).contains(&c.lists().len()),
        "4,000 vectors at a target of 500 gave {} lists",
        c.lists().len()
    );
}

#[test]
fn a_centroid_is_the_mean_of_what_it_holds() {
    // If a centroid drifts from its members, probe selection picks the wrong lists and
    // recall falls for a reason nothing in the search path can explain.
    let corpus = skewed(1_500, 6, 4, 5);
    let c = Clustering::build(&corpus, Params::default());
    for (ci, list) in c.lists().iter().enumerate() {
        if list.is_empty() {
            continue;
        }
        let centroid = &c.centroids()[ci];
        for (d, value) in centroid.iter().enumerate() {
            let mean: f32 = list.iter().map(|r| corpus[*r][d]).sum::<f32>() / list.len() as f32;
            assert!(
                (value - mean).abs() < 0.35,
                "centroid {ci} dim {d} is {value} but its members average {mean}"
            );
        }
    }
}

#[test]
fn the_same_corpus_clusters_the_same_way() {
    // Recall is compared across runs; a clustering that depends on iteration order makes
    // every comparison noise.
    let corpus = uniform(800, 6);
    let a = Clustering::build(&corpus, Params::default());
    let b = Clustering::build(&corpus, Params::default());
    assert_eq!(a.lists(), b.lists());
}

#[test]
fn a_corpus_smaller_than_one_list_makes_a_single_cluster() {
    let corpus = uniform(50, 7);
    let c = Clustering::build(&corpus, Params::default());
    assert_eq!(c.lists().len(), 1);
    assert_eq!(c.lists()[0].len(), 50);
}

#[test]
fn an_empty_corpus_is_not_a_panic() {
    let c = Clustering::build(&[], Params::default());
    assert!(c.lists().is_empty());
    assert!(c.centroids().is_empty());
}

#[test]
fn balance_is_bought_cheaply_not_at_any_price() {
    // ⚠️ Balance alone is trivially achievable: deal the vectors out round-robin and every
    // list is the same size. What makes a balanced clustering useful is that it is *also*
    // close to the assignment each vector would have chosen -- and nothing in the balance
    // bound says so.
    //
    // The fixture is GROUPED, not interleaved: documents arrive a topic at a time, so the
    // early rows fill the popular centroids and the late ones meet a full cap. Interleaved
    // arrival never makes the cap bind at all, and the assertion is vacuous -- measured,
    // not assumed, after this test first passed at a ratio of exactly 1.0000.
    let corpus = grouped(6_000, 10, 40, 3);
    let c = Clustering::build(
        &corpus,
        Params {
            target_list_size: 500,
            ..Params::default()
        },
    );
    let (got, floor) = (c.assignment_cost(&corpus), c.unconstrained_cost(&corpus));
    assert!(
        got <= floor * 1.5,
        "balanced assignment costs {got:.0} against an unconstrained floor of {floor:.0} \
         ({:.2}x): the capacity bound is being met by putting vectors in lists they have no \
         business being in",
        got / floor
    );
}
