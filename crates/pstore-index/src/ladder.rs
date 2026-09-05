//! The rerank ladder: oversample cheaply, then re-score the survivors (D-11, D-12).
//!
//! ⚠️ Each rung costs **bytes and one round trip**, never a data-dependent chain: the
//! survivors of rung *n* are known before rung *n+1* issues anything, so every fetch in a
//! rung goes out together. Width is free; depth is not.
//!
//! The ladder holds only the arithmetic of *how many to keep*. It does not fetch, does not
//! know what a segment is, and takes the scoring function as an argument — so it is
//! testable against brute force without a store, which is the only way to know a rung
//! actually improves the answer.

/// A precision tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    /// RaBitQ 1-bit codes, scored from the bytes a posting-list fetch already moved.
    OneBit,
    /// int8 codes, for the survivors only.
    Int8,
    /// Full precision, for the final few.
    Exact,
}

/// How many candidates survive each rung.
#[derive(Debug, Clone, Copy)]
pub struct Ladder {
    k: usize,
    oversample: usize,
}

impl Ladder {
    /// A ladder returning `k` results, oversampling by `oversample` at rung 0.
    #[must_use]
    pub fn new(k: usize, oversample: usize) -> Self {
        Self {
            k: k.max(1),
            oversample: oversample.max(1),
        }
    }

    /// How many candidates `rung` keeps.
    ///
    /// Strictly narrowing, because a rung that hands on as many as it received is a rescan
    /// at higher cost rather than a rerank.
    #[must_use]
    pub fn keep(&self, rung: Rung) -> usize {
        match rung {
            Rung::OneBit => self.k * self.oversample,
            // A fixed 2x, so the last rung's fetch is small enough to be worth its round
            // trip: at k=10 that is 20 vectors, tens of kilobytes.
            Rung::Int8 => self.k * 2,
            Rung::Exact => self.k,
        }
    }

    fn top(&self, mut scored: Vec<(usize, f32)>, keep: usize) -> Vec<(usize, f32)> {
        // Descending by score; ties broken by id so a result set is reproducible, which is
        // what lets a recall number be compared across runs at all.
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(keep);
        scored
    }

    /// Scores every candidate at 1-bit precision and keeps the oversampled top.
    pub fn rung0<I: IntoIterator<Item = (usize, f32)>>(&self, scored: I) -> Vec<(usize, f32)> {
        self.top(scored.into_iter().collect(), self.keep(Rung::OneBit))
    }

    /// Re-scores rung 0's survivors at int8 precision.
    pub fn rung1<F: FnMut(usize) -> f32>(
        &self,
        candidates: &[(usize, f32)],
        mut score: F,
    ) -> Vec<(usize, f32)> {
        let rescored = candidates.iter().map(|(i, _)| (*i, score(*i))).collect();
        self.top(rescored, self.keep(Rung::Int8))
    }

    /// Re-scores rung 1's survivors exactly.
    pub fn rung2<F: FnMut(usize) -> f32>(
        &self,
        candidates: &[(usize, f32)],
        mut score: F,
    ) -> Vec<(usize, f32)> {
        let rescored = candidates.iter().map(|(i, _)| (*i, score(*i))).collect();
        self.top(rescored, self.keep(Rung::Exact))
    }
}
