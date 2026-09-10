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
}

impl Default for Fusion {
    fn default() -> Self {
        Self::Rrf { k: 60.0 }
    }
}

/// Fuses each leg's ranked answer into one, keeping the best `top_k`.
///
/// ⚠️ **Order-independent.** The contributions are summed, and ties break on
/// `(segment, row)`, so shuffling the legs cannot change the ranking. Breaking ties by
/// insertion order instead would make the answer depend on the order a caller happened to
/// write its `prefetch[]`.
///
/// ⚠️ **Keyed on the pair, never on the row.** Rows are only unique inside a segment, so a
/// map keyed on the row alone silently merges two documents whenever two segments happen to
/// have a hit at the same ordinal — which, for a top-k over doc-ordered postings, is common
/// rather than rare.
#[must_use]
pub fn fuse(legs: &[Vec<Hit>], fusion: Fusion, top_k: usize) -> Vec<Hit> {
    let Fusion::Rrf { k } = fusion;
    let mut acc: std::collections::BTreeMap<(usize, usize), f32> =
        std::collections::BTreeMap::new();
    for leg in legs {
        for (i, hit) in leg.iter().enumerate() {
            // ⚠️ Rank is **1-based**. Zero-based makes the first hit worth `1/k` and the
            // second `1/(k+1)` — a smaller gap between first and second than between any
            // other pair, which is exactly backwards.
            #[expect(
                clippy::cast_precision_loss,
                reason = "a rank beyond 2^24 is a limit no leg reaches"
            )]
            let rank = (i + 1) as f32;
            *acc.entry((hit.segment, hit.row)).or_insert(0.0) += 1.0 / (k + rank);
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
