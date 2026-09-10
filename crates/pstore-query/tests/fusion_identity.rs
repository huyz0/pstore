//! ⚠️ Tests may panic: an assertion failure IS the reporting mechanism.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]

//! A hit names the segment it came from — M5f.1.
//!
//! ⚠️ **The defect this closes exists today and is reachable.** `fuse` accumulates by
//! `hit.row` into one map, and `Hit`'s own doc says the limit out loud: *"A **row**, not a
//! document id, and that is a stated limit rather than an oversight."* Stated, and not
//! enforced — so a caller who fuses two segments' legs gets segment 0's row 5 and segment 1's
//! row 5 **summed into one hit**, two unrelated documents merged at an inflated score, ranked
//! above both. Every leg is individually correct and nothing reports anything.

use pstore_query::{Fusion, Hit, fuse};

const K: f32 = 60.0;

#[test]
fn two_segments_with_the_same_row_are_two_hits() {
    let leg = vec![
        Hit {
            segment: 0,
            row: 5,
            score: 9.0,
        },
        Hit {
            segment: 1,
            row: 5,
            score: 8.0,
        },
    ];
    let out = fuse(&[leg], Fusion::Rrf { k: K }, 10);

    assert_eq!(
        out.len(),
        2,
        "row 5 of two different segments fused into {} hit(s) -- two unrelated documents \
         merged into one",
        out.len()
    );
    assert_eq!((out[0].segment, out[0].row), (0, 5));
    assert_eq!((out[1].segment, out[1].row), (1, 5));
    // ⚠️ The scores are the individual RRF contributions, not their sum. A merged hit would
    // carry 1/(k+1) + 1/(k+2) and outrank everything real.
    assert_eq!(out[0].score, 1.0 / (K + 1.0));
    assert_eq!(out[1].score, 1.0 / (K + 2.0));
}

#[test]
fn ties_break_on_the_pair_and_the_segment_comes_first() {
    // Two hits at the same rank in different legs, so their RRF contributions are equal and
    // the tie-break is the whole of the ordering.
    let a = vec![Hit {
        segment: 1,
        row: 2,
        score: 0.0,
    }];
    let b = vec![Hit {
        segment: 0,
        row: 9,
        score: 0.0,
    }];
    let out = fuse(&[a, b], Fusion::Rrf { k: K }, 10);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].score, out[1].score, "the fixture is not a tie");
    // ⚠️ `(0, 9)` before `(1, 2)`: the pair is ordered, segment first. Breaking on `row`
    // alone would put `(1, 2)` first and make the answer depend on how rows happen to fall
    // inside unrelated segments.
    assert_eq!((out[0].segment, out[0].row), (0, 9));
    assert_eq!((out[1].segment, out[1].row), (1, 2));
}

#[test]
fn shuffling_the_legs_still_cannot_change_the_ranking() {
    // The property `fuse`'s doc already claims, restated over the pair: contributions are
    // summed and ties break on `(segment, row)`, so leg order is not an input.
    let mk = |s: usize, r: usize| Hit {
        segment: s,
        row: r,
        score: 0.0,
    };
    let x = vec![mk(0, 1), mk(1, 1), mk(1, 0)];
    let y = vec![mk(1, 0), mk(0, 1)];
    let forward = fuse(&[x.clone(), y.clone()], Fusion::Rrf { k: K }, 10);
    let backward = fuse(&[y, x], Fusion::Rrf { k: K }, 10);
    assert_eq!(
        forward
            .iter()
            .map(|h| (h.segment, h.row))
            .collect::<Vec<_>>(),
        backward
            .iter()
            .map(|h| (h.segment, h.row))
            .collect::<Vec<_>>()
    );
}
