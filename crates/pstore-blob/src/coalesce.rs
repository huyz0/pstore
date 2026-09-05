//! Merging nearby byte ranges into one fetch.
//!
//! No cloud offers a multi-range GET we can rely on, which creates the central tension of
//! the read path: *n* small ranged GETs cost *n* round trips, while one large GET wastes
//! bytes. Since GETs are cheap, intra-region bandwidth is free, and latency is not, the
//! correct bias is to fetch the superset — bounded by `G*`, the gap at which transferring
//! the hole costs as much as an extra request.

use std::ops::Range;

/// One fetch, and which of the caller's ranges it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetch {
    /// The byte span to request from the backend.
    pub span: Range<u64>,
    /// Each served range with its index in the caller's slice, so results come back in
    /// the order asked and the reader never has to look up `ranges[i]` again — which is
    /// how the "index out of bounds" branch that could never fire gets deleted.
    pub serves: Vec<(usize, Range<u64>)>,
}

/// Plans the fetches for `ranges`, merging any two whose gap is below `gap`.
///
/// Empty ranges are dropped. The result never contains more fetches than there were
/// ranges, and every non-empty range is served by exactly one fetch.
#[must_use]
pub fn coalesce(ranges: &[Range<u64>], gap: u64) -> Vec<Fetch> {
    let mut idx: Vec<usize> = (0..ranges.len())
        .filter(|i| ranges.get(*i).is_some_and(|r| r.start < r.end))
        .collect();
    idx.sort_by_key(|i| ranges.get(*i).map(|r| r.start).unwrap_or(0));

    let mut out: Vec<Fetch> = Vec::new();
    for i in idx {
        let Some(r) = ranges.get(i) else { continue };
        match out.last_mut() {
            // `saturating_sub` because a later range may be *inside* the current span,
            // which is a gap of zero rather than an underflow.
            Some(f) if r.start.saturating_sub(f.span.end) < gap => {
                f.span.end = f.span.end.max(r.end);
                f.serves.push((i, r.clone()));
            }
            _ => out.push(Fetch {
                span: r.clone(),
                serves: vec![(i, r.clone())],
            }),
        }
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::single_range_in_vec_init,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_plans_nothing() {
        assert!(coalesce(&[], 16).is_empty());
    }

    #[test]
    fn empty_ranges_are_dropped() {
        // A zero-length range is not worth a request, and merging it would widen a span
        // for no reader.
        assert!(coalesce(&[5..5], 16).is_empty());
        assert_eq!(coalesce(&[0..4, 5..5], 16).len(), 1);
    }

    #[test]
    fn a_contained_range_does_not_underflow_the_gap() {
        // 0..100 then 10..20: `start - end` would underflow on unsigned arithmetic.
        let plan = coalesce(&[0..100, 10..20], 4);
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan.first().map(|f| f.span.clone()),
            Some(0..100),
            "the span must not shrink"
        );
    }

    #[test]
    fn a_zero_gap_merges_only_touching_ranges() {
        assert_eq!(
            coalesce(&[0..4, 4..8], 0).len(),
            2,
            "gap 0 disables merging"
        );
        assert_eq!(coalesce(&[0..4, 4..8], 1).len(), 1);
    }
}
