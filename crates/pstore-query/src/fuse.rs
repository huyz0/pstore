//! Reciprocal Rank Fusion — pure ranking, no I/O.

/// One row of one retriever's answer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// Which segment the row is in — an index into the segment list the query was given.
    ///
    /// ⚠️ **Without this, `fuse` merged rows across segments.** It accumulated by `row` into
    /// one map, so segment 0's row 5 and segment 1's row 5 became a single hit carrying the
    /// sum of their contributions: two unrelated documents fused into one, ranked above
    /// both, with every leg individually correct and nothing reporting anything.
    ///
    /// ⚠️ **One type, not a second `GlobalHit`.** The limit used to be stated here and its
    /// own wording said why a second type is worse — "inventing one twice is how the two come
    /// to disagree".
    ///
    /// ⚠️ Stable for the query's HEAD snapshot, which is all fusion needs. It is **not** an
    /// external handle: a compaction rewrites segments and moves rows.
    pub segment: usize,
    /// The row within that segment.
    pub row: usize,
    /// What the retriever scored it. Carried so a caller can see per-retriever scores, which
    /// D-29 requires for an external reranker; **RRF does not read it**.
    pub score: f32,
}

/// How several retrievers' answers become one (D-27).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fusion {
    /// Combine by **rank position**: `Σ 1/(k + rank)`.
    ///
    /// ⚠️ Deliberately blind to score magnitude. That is the price D-27 accepts: a retriever
    /// that knows A is *far* better than B cannot say so, and in exchange one retriever's
    /// score distribution drifting cannot silently take over the ranking. Weighted fusion is
    /// the escape hatch and is deliberately not the default — "equal weights until the
    /// customer has measured".
    Rrf {
        /// The classic 60. OQ-63 reports 10 as a tuned value in current practice; this is a
        /// parameter precisely so that question can be answered by measurement later.
        k: f32,
    },
    /// RRF with a weight per leg (M9g): each leg contributes `w / (k + rank)`. D-27's escape
    /// hatch, for a caller that has measured.
    WeightedRrf {
        /// As [`Fusion::Rrf`]'s.
        k: f32,
        /// One per leg, in leg order.
        weights: Weights,
    },
    /// The weighted **sum of raw leg scores** (M9g.2): a leg that did not score a row adds
    /// nothing, which is its score -- BM25 is never negative. ⚠️ **Text legs only**, and the
    /// caller enforces it (the server refuses a dense leg): a dense or sparse score can be
    /// negative, and a row that leg retrieved would then rank below one it never found. Its
    /// legs are whole, not cut at their limit: see `run`.
    Sum {
        /// One per leg, in leg order.
        weights: Weights,
    },
    /// The largest weighted raw leg score (M9g.2), text legs only as for [`Fusion::Sum`]. Legs
    /// keep their limit: a row's best leg ranks it within that leg's own top, so no cut loses
    /// it.
    Max {
        /// One per leg, in leg order.
        weights: Weights,
    },
}

/// The most legs a query may fuse (M9g.1): one dense leg and fifteen text legs, turbopuffer's
/// sixteen sub-queries.
pub const MAX_LEGS: usize = 16;

/// A weight per leg, at most [`MAX_LEGS`] of them -- fixed, so [`Fusion`] stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weights([f32; MAX_LEGS]);

impl Weights {
    /// Every leg weighing 1: what an unweighted `sum` or `max` is (M9g.2).
    pub const ONE: Self = Self([1.0; MAX_LEGS]);

    /// These weights, or `None` for more than [`MAX_LEGS`]. Legs past the given ones weigh 1.
    #[must_use]
    pub fn of(weights: &[f32]) -> Option<Self> {
        let mut all = [1.0; MAX_LEGS];
        all.get_mut(..weights.len())?.copy_from_slice(weights);
        Some(Self(all))
    }

    /// Whether the first `legs` weights are all zero (M9g.2).
    #[must_use]
    pub fn all_zero(&self, legs: usize) -> bool {
        self.0.iter().take(legs).all(|w| *w == 0.0)
    }

    /// Leg `i`'s weight.
    fn get(&self, i: usize) -> f32 {
        self.0.get(i).copied().unwrap_or(1.0)
    }
}

impl Default for Fusion {
    fn default() -> Self {
        Self::Rrf { k: 60.0 }
    }
}

/// Fuses each leg's ranked answer into one, keeping the best `top_k`.
///
/// ⚠️ **Order-independent, until weights name legs by position** (M9g). The contributions are
/// summed, and ties break on `(segment, row)`, so shuffling the legs cannot change an
/// unweighted ranking. Breaking ties by insertion order instead would make the answer depend
/// on the order a caller happened to write its `prefetch[]`. A weighted fusion's weights are
/// in leg order, so there the order is the caller's, by construction.
///
/// ⚠️ **Keyed on the pair, never on the row.** Rows are only unique inside a segment, so a
/// map keyed on the row alone silently merges two documents whenever two segments happen to
/// have a hit at the same ordinal — which, for a top-k over doc-ordered postings, is common
/// rather than rare.
#[must_use]
pub fn fuse(legs: &[Vec<Hit>], fusion: Fusion, top_k: usize) -> Vec<Hit> {
    let mut acc: std::collections::BTreeMap<(usize, usize), f32> =
        std::collections::BTreeMap::new();
    for (j, leg) in legs.iter().enumerate() {
        for (i, hit) in leg.iter().enumerate() {
            // ⚠️ Rank is **1-based**. Zero-based makes the first hit worth `1/k` and the
            // second `1/(k+1)` — a smaller gap between first and second than between any
            // other pair, which is exactly backwards.
            #[expect(
                clippy::cast_precision_loss,
                reason = "a rank beyond 2^24 is a limit no leg reaches"
            )]
            let rank = (i + 1) as f32;
            let slot = acc.entry((hit.segment, hit.row));
            match fusion {
                Fusion::Rrf { k } => *slot.or_insert(0.0) += 1.0 / (k + rank),
                Fusion::WeightedRrf { k, weights } => {
                    *slot.or_insert(0.0) += weights.get(j) / (k + rank);
                }
                Fusion::Sum { weights } => *slot.or_insert(0.0) += weights.get(j) * hit.score,
                // Only the legs that scored the row: an absent leg is not a zero.
                Fusion::Max { weights } => {
                    let v = weights.get(j) * hit.score;
                    let best = slot.or_insert(v);
                    *best = best.max(v);
                }
            }
        }
    }
    let mut out: Vec<Hit> = acc
        .into_iter()
        .map(|((segment, row), score)| Hit {
            segment,
            row,
            score,
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| (a.segment, a.row).cmp(&(b.segment, b.row)))
    });
    out.truncate(top_k);
    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    #[test]
    fn weights_fill_to_one_and_refuse_past_the_bound() {
        let w = Weights::of(&[2.0, 0.5]).unwrap();
        assert_eq!(
            (w.get(0), w.get(1), w.get(2), w.get(MAX_LEGS - 1)),
            (2.0, 0.5, 1.0, 1.0)
        );
        assert!(Weights::of(&[1.0; MAX_LEGS]).is_some());
        assert!(Weights::of(&[1.0; MAX_LEGS + 1]).is_none());
        let zeros = Weights::of(&[0.0, 0.0, 3.0]).unwrap();
        assert!(zeros.all_zero(2));
        assert!(!zeros.all_zero(3));
    }
}
