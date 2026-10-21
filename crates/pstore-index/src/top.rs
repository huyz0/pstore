//! Top-k selection and a dense score accumulator, shared by every ranked leg (M119).
//!
//! ⚠️ **Exact, not approximate.** Every caller ranks by a total order -- `total_cmp` on the
//! score, then a unique id -- so the `k` smallest under it are one set, whichever way they
//! are found, and sorting that set gives what sorting everything and truncating did. Two
//! elements the order calls equal are equal in every field, so stability cannot show.

use std::cmp::Ordering;

/// `v` sorted by `cmp` and cut to its first `k`, in `O(n + k log k)` rather than
/// `O(n log n)`.
pub fn top_k_by<T, F>(v: &mut Vec<T>, k: usize, mut cmp: F)
where
    F: FnMut(&T, &T) -> Ordering,
{
    if k == 0 {
        v.clear();
        return;
    }
    if v.len() > k {
        v.select_nth_unstable_by(k - 1, &mut cmp);
        v.truncate(k);
    }
    v.sort_unstable_by(cmp);
}

/// `(row, score)` pairs best first: score descending, then row ascending.
#[must_use]
pub fn best_first(a: &(usize, f32), b: &(usize, f32)) -> Ordering {
    b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0))
}

/// Scores summed per row, for rows that are dense small integers.
///
/// ⚠️ It replaces a `HashMap<u32, f32>` with `*entry(row).or_insert(0.0) += x`, and agrees with
/// it to the bit: each row starts from `+0.0` and adds the same terms in the same order, and
/// a row touched only by zeros is still reported.
#[derive(Debug, Default)]
pub struct Accumulator {
    scores: Vec<f32>,
    touched: Vec<bool>,
    rows: Vec<u32>,
    /// Rows past [`DENSE_ROWS`], which no real segment holds: a corrupt posting naming row
    /// 4 billion costs a map entry, not 16 GB.
    far: std::collections::HashMap<u32, f32>,
}

/// The largest row kept densely: far above any segment's row count.
pub(crate) const DENSE_ROWS: usize = 1 << 22;

impl Accumulator {
    /// An accumulator expecting rows below `rows`; it grows past that if asked.
    #[must_use]
    pub fn with_rows(rows: usize) -> Self {
        let rows = rows.min(DENSE_ROWS);
        Self {
            scores: vec![0.0; rows],
            touched: vec![false; rows],
            rows: Vec::new(),
            far: std::collections::HashMap::new(),
        }
    }

    /// Adds `x` to `row`'s score.
    pub fn add(&mut self, row: u32, x: f32) {
        let at = row as usize;
        if at >= DENSE_ROWS {
            *self.far.entry(row).or_insert(0.0) += x;
            return;
        }
        if at >= self.scores.len() {
            self.scores.resize(at + 1, 0.0);
            self.touched.resize(at + 1, false);
        }
        if let (Some(s), Some(t)) = (self.scores.get_mut(at), self.touched.get_mut(at)) {
            if !*t {
                *t = true;
                self.rows.push(row);
            }
            *s += x;
        }
    }

    /// Every touched row with its score, in no particular order.
    #[must_use]
    pub fn into_scored(self) -> Vec<(usize, f32)> {
        let scores = self.scores;
        self.rows
            .into_iter()
            .map(|r| (r as usize, scores.get(r as usize).copied().unwrap_or(0.0)))
            .chain(self.far.into_iter().map(|(r, s)| (r as usize, s)))
            .collect()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    /// SplitMix64, so a failure is reproducible.
    fn rng(mut s: u64) -> impl FnMut() -> u64 {
        move || {
            s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
    }

    #[test]
    fn top_k_matches_a_full_sort() {
        let mut next = rng(7);
        for n in [0usize, 1, 2, 5, 10, 11, 64, 1_000] {
            // Few distinct scores, so ties are common and the id decides them; NaN and the
            // two zeros, which `total_cmp` orders.
            let v: Vec<(usize, f32)> = (0..n)
                .map(|i| {
                    let s = match next() % 7 {
                        0 => f32::NAN,
                        1 => -0.0,
                        2 => 0.0,
                        x => x as f32,
                    };
                    (i, s)
                })
                .collect();
            for k in [0usize, 1, 3, 10, n, n + 1] {
                let mut want = v.clone();
                want.sort_by(best_first);
                want.truncate(k);
                let mut got = v.clone();
                top_k_by(&mut got, k, best_first);
                let bits = |x: &[(usize, f32)]| -> Vec<(usize, u32)> {
                    x.iter().map(|(i, s)| (*i, s.to_bits())).collect()
                };
                assert_eq!(bits(&got), bits(&want), "n {n} k {k}");
            }
        }
    }

    #[test]
    fn the_dense_accumulator_is_the_map() {
        let mut next = rng(11);
        let adds: Vec<(u32, f32)> = (0..5_000)
            .map(|_| {
                let row = (next() % 300) as u32;
                // Zeros included: a row touched only by zeros must still be reported.
                let x = if next().is_multiple_of(5) {
                    0.0
                } else {
                    (next() % 1_000) as f32 / 7.0
                };
                (row, x)
            })
            // And rows past the dense bound, kept in the map beside it.
            .chain([(999, 0.0), (1_200, 1.5), (u32::MAX, 0.0), (u32::MAX, 2.0)])
            .collect();
        let mut map: std::collections::HashMap<u32, f32> = std::collections::HashMap::new();
        // Sized below the largest row, so growing is exercised.
        let mut acc = Accumulator::with_rows(100);
        for (row, x) in &adds {
            *map.entry(*row).or_insert(0.0) += x;
            acc.add(*row, *x);
        }
        let mut want: Vec<(usize, u32)> = map
            .into_iter()
            .map(|(r, s)| (r as usize, s.to_bits()))
            .collect();
        want.sort_unstable();
        let mut got: Vec<(usize, u32)> = acc
            .into_scored()
            .into_iter()
            .map(|(r, s)| (r, s.to_bits()))
            .collect();
        got.sort_unstable();
        assert_eq!(got, want);
    }
}
