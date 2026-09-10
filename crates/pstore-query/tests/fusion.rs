//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! RRF, as arithmetic. No store, no segment — the ranking rule on its own.

use pstore_query::{Fusion, Hit, fuse};

fn leg(rows: &[usize]) -> Vec<Hit> {
    rows.iter()
        .enumerate()
        .map(|(i, r)| Hit {
            // One segment: this file is the ranking rule on its own, and the cross-segment
            // identity is `fusion_identity.rs`.
            segment: 0,
            row: *r,
            // Deliberately descending and deliberately ignored: RRF reads RANK, and a
            // fusion that quietly read this instead would rank identically on any fixture
            // whose scores already agree with its ranks.
            score: 100.0 - i as f32,
        })
        .collect()
}

#[test]
fn rrf_is_the_reciprocal_rank_sum() {
    // ⚠️ Criterion 1, hand-computed. Row 7 is first in one leg and second in the other:
    //   1/(60+1) + 1/(60+2) = 0.016393 + 0.016129 = 0.032522
    // Row 3 is second and first: the same sum, so it must tie and break on the row. Row 9
    // appears once: 1/(60+3) = 0.015873.
    let out = fuse(&[leg(&[7, 3, 9]), leg(&[3, 7])], Fusion::default(), 10);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].row, 3, "a tie did not break on the row");
    assert_eq!(out[1].row, 7);
    assert_eq!(out[2].row, 9);
    assert!(
        (out[0].score - 0.032_522).abs() < 1e-5,
        "RRF is not 1/(k+rank): got {}",
        out[0].score
    );
    assert!(
        (out[2].score - 0.015_873).abs() < 1e-5,
        "a single-leg row scored {}",
        out[2].score
    );
}

#[test]
fn k_is_a_parameter_and_changes_the_answer() {
    // ⚠️ `k` ignored is the mutation this kills, and it is invisible to criterion 1: at any
    // fixed k the ORDER of a two-leg fusion is usually the same. What k controls is how much
    // one leg's top hit can outweigh agreement further down, so the fixture is built on
    // exactly that trade.
    let legs = [leg(&[1]), leg(&[2, 1])];
    let small = fuse(&legs, Fusion::Rrf { k: 0.5 }, 10);
    let large = fuse(&legs, Fusion::Rrf { k: 1000.0 }, 10);
    assert_eq!(
        small[0].row, 1,
        "at k=0.5 the row in both legs must still win"
    );
    assert!(
        small[0].score > large[0].score * 10.0,
        "k did not change the scale of the answer: {} against {}",
        small[0].score,
        large[0].score
    );
}

#[test]
fn agreement_between_legs_outranks_a_single_leg() {
    // ⚠️ Criterion 2, and the property that makes hybrid worth having at all. Row 5 is
    // SECOND in both legs; row 1 is first in one and absent from the other. Agreement must
    // win. A fusion taking `max` of the contributions instead of the sum ranks row 1 first
    // and passes every other test in this file.
    let out = fuse(&[leg(&[1, 5]), leg(&[2, 5])], Fusion::default(), 10);
    assert_eq!(
        out[0].row, 5,
        "a row found by both retrievers lost to one found by one"
    );
}

#[test]
fn fusion_does_not_depend_on_leg_order() {
    // ⚠️ Criterion 3. Ties broken by insertion order make the answer depend on the order a
    // caller happened to write its `prefetch[]` — reproducible on one machine, and different
    // the moment a client reorders its request.
    let a = leg(&[4, 8, 1, 6]);
    let b = leg(&[8, 1, 9]);
    let c = leg(&[6, 4]);
    let one = fuse(&[a.clone(), b.clone(), c.clone()], Fusion::default(), 10);
    let two = fuse(&[c, a, b], Fusion::default(), 10);
    assert_eq!(
        one.iter().map(|h| h.row).collect::<Vec<_>>(),
        two.iter().map(|h| h.row).collect::<Vec<_>>(),
        "shuffling the legs changed the ranking"
    );
    for (x, y) in one.iter().zip(&two) {
        assert_eq!(x.score, y.score, "shuffling the legs changed a score");
    }
}

#[test]
fn top_k_bounds_the_fused_answer_and_no_leg() {
    // `top_k` is over the fused list. Applied per leg instead, one retriever's cheap rows
    // crowd out the other's before fusion ever sees them.
    let out = fuse(
        &[leg(&[1, 2, 3, 4, 5]), leg(&[6, 7, 8])],
        Fusion::default(),
        3,
    );
    assert_eq!(out.len(), 3);
    let empty: Vec<Hit> = Vec::new();
    assert!(fuse(&[empty], Fusion::default(), 5).is_empty());
    assert!(fuse(&[], Fusion::default(), 5).is_empty());
}
