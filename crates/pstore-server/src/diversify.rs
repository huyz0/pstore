//! A ranked window diversified by maximal marginal relevance (M64).
//!
//! ⚠️ **Similarity is the cosine of the vectors, for every metric, and deliberately not the
//! metric's own.** MMR asks whether two results are alike, which is direction; the ranking
//! under dot product or euclidean also weighs magnitude. The vectors come in client space,
//! from `Answer::vectors`.

/// A parsed `diversify`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Diversify {
    /// 1 is relevance alone, 0 diversity alone.
    pub(crate) lambda: f64,
    /// Candidates the engine ranks, and the selection picks from.
    pub(crate) window: usize,
}

/// The `window` of `raw`, if it names one as a whole number.
pub(crate) fn named_window(raw: &serde_json::Value) -> Option<usize> {
    raw.get("window")
        .and_then(serde_json::Value::as_u64)
        .and_then(|w| usize::try_from(w).ok())
}

/// `raw` as a diversify for a query of `top_k`, or why it is not one. `shared` is the window
/// a boost named: one window governs both.
///
/// # Errors
/// Every refusal of M64's rule 6, as a message for a `400`.
pub(crate) fn parse(
    raw: &serde_json::Value,
    top_k: usize,
    shared: Option<usize>,
) -> Result<Diversify, String> {
    let o = raw
        .as_object()
        .ok_or("diversify is {\"lambda\", \"window\"}")?;
    if let Some(k) = o.keys().find(|k| *k != "lambda" && *k != "window") {
        return Err(format!(
            "{k:?} is not a diversify field; lambda and window are"
        ));
    }
    let lambda = o
        .get("lambda")
        .and_then(serde_json::Value::as_f64)
        .filter(|l| (0.0..=1.0).contains(l))
        .ok_or("lambda is a number from 0 to 1")?;
    let window = match o.get("window") {
        None => shared.unwrap_or_else(|| crate::boost::default_window(top_k)),
        Some(w) => w
            .as_u64()
            .and_then(|w| usize::try_from(w).ok())
            .ok_or("window is a whole number")?,
    };
    if window < top_k || window > crate::boost::MAX_WINDOW {
        return Err(format!(
            "window is from top_k ({top_k}) to {}, not {window}",
            crate::boost::MAX_WINDOW
        ));
    }
    Ok(Diversify { lambda, window })
}

/// The cosine of two vectors, in `f64`: 0 when either is missing or zero, or their dimensions
/// differ. Never clamped: opposite vectors have similarity -1, and that is a bonus.
fn cosine(a: Option<&Vec<f32>>, b: Option<&Vec<f32>>) -> f64 {
    let (Some(a), Some(b)) = (a, b) else {
        return 0.0;
    };
    if a.len() != b.len() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// The indices of the `k` candidates MMR picks, in pick order.
///
/// `scores` and `vectors` are the window's, in the engine's order, and in `f64` throughout.
/// Each pick maximizes `lambda * rel - (1 - lambda) * m`, where `rel` is the score scaled to
/// `0..=1` over the window (1 for all when the range is flat or not finite) and `m` is the
/// greatest similarity to anything already picked, 0 while nothing is. **Strictly greater
/// wins, so the earlier candidate keeps a tie.** The maximum is kept running, updated as each
/// pick lands, so the work is `k × window` similarities, not `k² × window`.
pub(crate) fn select(
    scores: &[f64],
    vectors: &[Option<Vec<f32>>],
    lambda: f64,
    k: usize,
) -> Vec<usize> {
    let n = scores.len();
    let finite = scores.iter().all(|s| s.is_finite());
    let (lo, hi) = scores
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), s| {
            (lo.min(*s), hi.max(*s))
        });
    let rel: Vec<f64> = scores
        .iter()
        .map(|s| {
            if finite && hi > lo {
                (s - lo) / (hi - lo)
            } else {
                1.0
            }
        })
        .collect();
    let mut nearest: Vec<Option<f64>> = vec![None; n];
    let mut picked = vec![false; n];
    let mut order = Vec::with_capacity(k.min(n));
    while order.len() < k.min(n) {
        let mut best: Option<(f64, usize)> = None;
        for (d, (r, near)) in rel.iter().zip(&nearest).enumerate() {
            if picked.get(d).copied().unwrap_or(true) {
                continue;
            }
            let objective = lambda * r - (1.0 - lambda) * near.unwrap_or(0.0);
            if best.is_none_or(|(b, _)| objective > b) {
                best = Some((objective, d));
            }
        }
        let Some((_, p)) = best else { break };
        if let Some(slot) = picked.get_mut(p) {
            *slot = true;
        }
        order.push(p);
        let chosen = vectors.get(p).and_then(Option::as_ref);
        for (d, near) in nearest.iter_mut().enumerate() {
            if picked.get(d).copied().unwrap_or(true) {
                continue;
            }
            let s = cosine(vectors.get(d).and_then(Option::as_ref), chosen);
            *near = Some(near.map_or(s, |m| m.max(s)));
        }
    }
    order
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v(x: &[f32]) -> Option<Vec<f32>> {
        Some(x.to_vec())
    }

    #[test]
    fn similarity_is_zero_without_a_usable_vector() {
        // Scores [3, 1, 2] are relevances 1, 0, 0.5. After the first pick, a candidate that
        // is penalized as a duplicate (similarity 1) drops below one that is not.
        let scores = [3.0, 1.0, 2.0];
        // A candidate of another dimension: similarity 0, so it is not penalized and, at
        // objective 0, beats the penalized duplicate's -0.25. Truncating by `zip` would read
        // it as a duplicate (similarity 1, objective -0.5) and pick the other first.
        let other_dims = [v(&[1.0, 0.0]), v(&[1.0, 0.0, 0.0]), v(&[1.0, 0.0])];
        assert_eq!(select(&scores, &other_dims, 0.5, 3), [0, 1, 2]);
        // No vector at all.
        let none = [v(&[1.0, 0.0]), None, v(&[1.0, 0.0])];
        assert_eq!(select(&scores, &none, 0.5, 3), [0, 1, 2]);
        // A zero vector: 0/0 must not become NaN and poison the comparison.
        let zero = [v(&[1.0, 0.0]), v(&[0.0, 0.0]), v(&[1.0, 0.0])];
        assert_eq!(select(&scores, &zero, 0.5, 3), [0, 1, 2]);
        // A zero vector that outranks the one before it in the window: one zero norm alone must
        // give similarity 0 (0/0 is NaN, and a NaN objective never beats a number), so it is
        // picked second.
        let zero_last = [v(&[1.0, 0.0]), v(&[0.0, 1.0]), v(&[0.0, 0.0])];
        assert_eq!(select(&scores, &zero_last, 0.5, 3), [0, 2, 1]);
        // Control: all three alike, so the duplicate is penalized and the order is the other.
        let alike = [v(&[1.0, 0.0]), v(&[1.0, 0.0]), v(&[1.0, 0.0])];
        assert_eq!(select(&scores, &alike, 0.5, 3), [0, 2, 1]);
    }

    #[test]
    fn a_negative_similarity_is_a_bonus() {
        // Relevances 1, 0, 0.5. After the first pick the opposite vector (similarity -1)
        // scores 0 + 0.5, beating the orthogonal one's 0.25. Clamped at 0 it would lose.
        let scores = [3.0, 1.0, 2.0];
        let vectors = [v(&[1.0, 0.0]), v(&[-1.0, 0.0]), v(&[0.0, 1.0])];
        assert_eq!(select(&scores, &vectors, 0.5, 3), [0, 1, 2]);
    }

    #[test]
    fn a_range_that_is_flat_or_not_finite_gives_every_candidate_relevance_one() {
        // Relevance all 1: only similarity decides after the first pick, which is the first.
        let vectors = [v(&[1.0, 0.0]), v(&[1.0, 0.0]), v(&[0.0, 1.0])];
        for scores in [
            [1.0, 1.0, 1.0],
            [f64::NAN, 2.0, 1.0],
            [f64::INFINITY, 2.0, 1.0],
            [3.0, f64::NEG_INFINITY, 1.0],
        ] {
            assert_eq!(select(&scores, &vectors, 0.5, 3), [0, 2, 1], "{scores:?}");
        }
    }

    #[test]
    fn lambda_one_is_relevance_and_lambda_zero_starts_with_the_first() {
        let scores = [1.0, 3.0, 2.0];
        let vectors = [v(&[1.0, 0.0]), v(&[1.0, 0.0]), v(&[0.0, 1.0])];
        assert_eq!(select(&scores, &vectors, 1.0, 3), [1, 2, 0]);
        // Lambda 0: every first objective is 0, so index 0; then the least similar, 2; then 1.
        assert_eq!(select(&scores, &vectors, 0.0, 3), [0, 2, 1]);
    }

    #[test]
    fn ties_keep_the_earlier_candidate_and_a_short_window_returns_what_it_has() {
        let scores = [1.0, 1.0];
        let vectors = [v(&[1.0, 0.0]), v(&[1.0, 0.0])];
        assert_eq!(select(&scores, &vectors, 0.5, 5), [0, 1]);
        assert!(select(&[], &[], 0.5, 5).is_empty());
        assert!(select(&scores, &vectors, 0.5, 0).is_empty());
    }

    #[test]
    fn parse_takes_the_shared_window_and_refuses_what_it_should() {
        let top_k = 5;
        let d = parse(&json!({"lambda": 0.5}), top_k, None).unwrap();
        assert_eq!(d.window, 100);
        assert_eq!(
            parse(&json!({"lambda": 0.5}), top_k, Some(40))
                .unwrap()
                .window,
            40
        );
        assert_eq!(
            parse(&json!({"lambda": 0, "window": 7}), top_k, Some(40))
                .unwrap()
                .window,
            7
        );
        assert_eq!(
            parse(&json!({"lambda": 1, "window": 5}), top_k, None)
                .unwrap()
                .window,
            5
        );
        assert_eq!(
            parse(&json!({"lambda": 1, "window": 1000}), top_k, None)
                .unwrap()
                .window,
            1000
        );
        for bad in [
            json!(0.5),
            json!({}),
            json!({"lambda": "0.5"}),
            json!({"lambda": 1.01}),
            json!({"lambda": -0.01}),
            json!({"lambda": 0.5, "window": 4}),
            json!({"lambda": 0.5, "window": 1001}),
            json!({"lambda": 0.5, "window": 10.5}),
            json!({"lambda": 0.5, "k": 1}),
        ] {
            assert!(parse(&bad, top_k, None).is_err(), "{bad}");
        }
        assert!(
            parse(&json!({"lambda": 0.5}), 1001, None).is_err(),
            "top_k above the cap"
        );
    }
}
