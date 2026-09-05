//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! Recall, measured against brute force.
//!
//! ⚠️ Every number here carries its **scale, dimension and dataset**, because a recall
//! figure without them is not a measurement (`evaluation-methodology.md`). These run inside
//! `cargo test`, so they are deliberately small; the gate-scale sweep is `scripts/recall.sh`.

use pstore_index::cluster::Params;
use pstore_index::ladder::Ladder;
use pstore_index::search;

mod common;
use common::{Corpus, recall_at, rung0_scores};

const DIM: usize = 128;

#[test]
fn the_clustered_index_meets_the_recall_floor() {
    // ⚠️ A SMOKE test at 4,000 x 128d, not the gate. The gate is `scripts/recall.sh` at
    // 20,000 x 384d, because a gate-scale corpus here is rebuilt once per mutant by
    // `cargo mutants`. This catches a collapse; the gate catches a regression.
    let c = Corpus::clustered(4_000, DIM, 20, 1);
    let idx = c.build(Params {
        target_list_size: 200,
        ..Params::default()
    });
    let r = recall_at(&c, &idx, 10, 16, 32);
    assert!(
        r >= 0.90,
        "recall@10 was {r:.3} on 4,000 x {DIM}d clustered synthetic at p=16, oversample=32"
    );
}

#[test]
fn probing_every_list_reaches_every_candidate() {
    // ⚠️ Candidate-set COMPLETENESS, not recall 1.0. With 1-bit scoring and a finite
    // oversample the true neighbour can still fall below the cutoff, so demanding perfect
    // recall here would be demanding the quantizer be exact. What must hold is that no
    // vector is unreachable: a row in no probed list is lost at any `p`, and shows up only
    // as a recall ceiling that will not lift however wide the probe.
    let c = Corpus::clustered(2_000, DIM, 20, 2);
    let idx = c.build(Params {
        target_list_size: 250,
        ..Params::default()
    });
    let all = search::candidates(
        &idx.clustering,
        &(0..idx.clustering.lists().len()).collect::<Vec<_>>(),
    );
    assert_eq!(
        all.len(),
        c.vectors.len(),
        "some rows are in no list at all"
    );
}

#[test]
fn augmentation_lifts_recall_by_five_points_at_p_two() {
    // ⚠️ Criterion 9. Boundary replication only shows up at a SMALL probe count -- at a
    // wide `p` the query reaches the neighbouring list anyway and augmentation is pure
    // index size. p=2 is where the vectors on the wrong side of a boundary are lost.
    // ⚠️ 40 lists over 12 groups, matching the configuration the boundary parameter was
    // measured on. A smaller fixture has fewer lists, so more of each neighbourhood already
    // sits in the probed list and the baseline is high enough that augmentation has little
    // left to add -- it measured 4.5 points on 1,500 vectors and 9.2 on this shape. A test
    // whose fixture removes the effect it is testing is not a cheaper test.
    let c = Corpus::clustered(3_000, DIM, 12, 3);
    let base = Params {
        target_list_size: 75,
        ..Params::default()
    };
    let without = c.build(Params {
        replicas: 0,
        ..base
    });
    let with = c.build(base);

    let r0 = recall_at(&c, &without, 10, 2, 32);
    let r1 = recall_at(&c, &with, 10, 2, 32);
    assert!(
        r1 - r0 >= 0.05,
        "augmentation moved recall@10 at p=2 from {r0:.3} to {r1:.3}, under the 5-point \
         floor: it is costing index size and buying nothing"
    );
}

#[test]
fn more_probes_never_lose_recall() {
    // Monotonicity is what makes `p` a knob rather than a lottery. A search that reordered
    // or dropped candidates as `p` grew would still look fine on any single measurement.
    let c = Corpus::clustered(3_000, DIM, 20, 4);
    let idx = c.build(Params {
        target_list_size: 100,
        ..Params::default()
    });
    let mut last = 0.0;
    for p in [1usize, 2, 4, 8, 16, 32] {
        let r = recall_at(&c, &idx, 10, p, 32);
        assert!(
            r >= last - 1e-9,
            "recall fell from {last:.3} to {r:.3} when p rose to {p}"
        );
        last = r;
    }
    assert!(last > 0.95, "recall at p=32 was only {last:.3}");
}

#[test]
fn an_easy_query_prunes_more_than_a_hard_one() {
    // ⚠️ Criterion 10. A query sitting on a centroid has one obviously-right list; one
    // equidistant between centroids genuinely needs several. Pruning that returns the same
    // set for both is pruning that does nothing.
    let c = Corpus::clustered(3_000, DIM, 20, 5);
    let idx = c.build(Params {
        target_list_size: 100,
        ..Params::default()
    });
    let easy = idx.clustering.centroids()[0].clone();
    // Midway between two centroids: equidistant by construction.
    let hard: Vec<f32> = idx.clustering.centroids()[0]
        .iter()
        .zip(&idx.clustering.centroids()[1])
        .map(|(a, b)| (a + b) / 2.0)
        .collect();

    let e = search::probe_pruned(&idx.clustering, &easy, 16, 0.25).len();
    let h = search::probe_pruned(&idx.clustering, &hard, 16, 0.25).len();
    assert!(
        e < h,
        "pruning kept {e} lists for a query on a centroid and {h} for one between two: it \
         is not query-aware"
    );
    // And it never returns nothing, which would turn a hard query into no answer at all.
    assert!(e >= 1);
}

#[test]
fn the_ladder_only_helps() {
    // Rungs cost bytes and a round trip; if they do not raise recall they should not exist.
    let c = Corpus::clustered(3_000, DIM, 20, 6);
    let idx = c.build(Params {
        target_list_size: 100,
        ..Params::default()
    });
    let l = Ladder::new(10, 32);
    let (mut rung0, mut rung2) = (0usize, 0usize);
    for qi in 0..40 {
        let query = &c.queries[qi];
        let want = c.truth(query, 10);
        let scored = l.rung0(rung0_scores(&c, &idx, query, 8));
        rung0 += want
            .iter()
            .filter(|i| scored.iter().take(10).any(|(j, _)| j == *i))
            .count();
        let exact = l.rung2(&scored, |r| c.dot(r, query));
        rung2 += want
            .iter()
            .filter(|i| exact.iter().any(|(j, _)| j == *i))
            .count();
    }
    assert!(
        rung2 >= rung0,
        "exact rerank lost ground: {rung2} against {rung0}"
    );
}
