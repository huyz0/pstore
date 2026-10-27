//! The bound on one by-filter operation (M136): the rows its filter may admit before the
//! fold refuses it whole, and the counts that keep what a fold holds for it bounded.

use pstore_format::Document;
use pstore_query::Predicate;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The most rows one by-filter operation may admit, by default.
pub(crate) const FILTER_ROWS: usize = 100_000;

/// What one by-filter operation counts: the rows its filter admits, or, for a partial patch
/// (M137), only those of them it would change.
pub(crate) struct Counted {
    pub(crate) filter: Predicate,
    /// The partial patch, when it is one.
    pub(crate) changes: Option<Document>,
    /// Its own bound, when it carries one (M139); the smaller of it and the engine's applies.
    pub(crate) rows: Option<usize>,
}

impl Counted {
    fn counts(&self, d: &Document) -> bool {
        self.filter.admits(&d.id, &d.attrs)
            && self
                .changes
                .as_ref()
                .is_none_or(|op| !crate::same_row(&crate::merged(d.clone(), op), d))
    }
}

/// Counts, per by-filter operation, the current rows its filter admits as a fold scans the
/// index, and says which of them to retain.
///
/// ⚠️ **Atomics**, because segments are read concurrently: which rows past the bound were
/// retained varies between runs, and does not matter -- an operation past it is refused.
pub(crate) struct Retention {
    /// Each by-filter operation's filter, in arrival order; `None` for one the fold cannot
    /// read, which admits nothing and counts nothing.
    filters: Vec<Option<Counted>>,
    counts: Vec<AtomicUsize>,
    bound: usize,
}

impl Retention {
    pub(crate) fn new(filters: Vec<Option<Counted>>, bound: usize) -> Self {
        let counts = filters.iter().map(|_| AtomicUsize::new(0)).collect();
        Self {
            filters,
            counts,
            bound,
        }
    }

    /// An operation's bound: the engine's, or its own when smaller (M139).
    fn bound_of(&self, f: &Counted) -> usize {
        f.rows.map_or(self.bound, |r| r.min(self.bound))
    }

    /// Counts `d` against every filter that admits it, and whether to retain it for them:
    /// one of them is still within its bound. Every filter is counted, never short-circuited.
    pub(crate) fn admit(&self, d: &Document) -> bool {
        let mut keep = false;
        for (f, n) in self.filters.iter().zip(&self.counts) {
            if let Some(f) = f.as_ref().filter(|f| f.counts(d)) {
                keep |= n.fetch_add(1, Ordering::Relaxed) < self.bound_of(f);
            }
        }
        keep
    }

    /// Per operation, in arrival order, whether its filter admitted more than the bound.
    pub(crate) fn over(&self) -> Vec<bool> {
        self.filters
            .iter()
            .zip(&self.counts)
            .map(|(f, n)| {
                f.as_ref()
                    .is_some_and(|f| n.load(Ordering::Relaxed) > self.bound_of(f))
            })
            .collect()
    }
}

/// What `by_filter` needs of the bound (M137): the bound, and whether the scan found the
/// operation past it.
#[derive(Clone, Copy)]
pub(crate) struct Limit {
    pub(crate) rows: usize,
    pub(crate) over: bool,
}

/// What `resolve` needs of the bound: per by-filter operation in arrival order, whether the
/// scan found it past the bound, and the bound itself, for the count in fold order.
pub(crate) struct Bound<'a> {
    pub(crate) over: &'a [bool],
    pub(crate) rows: usize,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "assertions in tests")]
    use super::*;
    use pstore_format::Value;
    use pstore_query::Op;

    fn row(i: i64) -> Document {
        let mut d = Document::new(format!("r{i}"), vec![1.0]);
        d.attrs.insert("n".to_owned(), Value::Int(i % 2));
        d
    }

    fn n_is(n: i64) -> Option<Counted> {
        Some(Counted {
            filter: Predicate::Cmp("n".to_owned(), Op::Eq, Value::Int(n)),
            changes: None,
            rows: None,
        })
    }

    #[test]
    fn retention_stops_at_the_bound() {
        // Ten rows, five with n = 0 and five with n = 1; a bound of 3.
        let r = Retention::new(vec![n_is(0), None, n_is(1), n_is(0)], 3);
        let kept = (0..10).filter(|&i| r.admit(&row(i))).count();
        // n = 0 is admitted by two operations, so retained while either is within: 3, and
        // 3 for n = 1.
        assert_eq!(kept, 6);
        assert_eq!(r.over(), [true, false, true, true]);
    }

    #[test]
    fn the_bound_itself_is_within() {
        let r = Retention::new(vec![n_is(1)], 5);
        assert!((0..10).all(|i| i % 2 == 0 || r.admit(&row(i))));
        assert_eq!(r.over(), [false]);
        assert!(!r.admit(&row(11)));
        assert_eq!(r.over(), [true]);
    }

    #[test]
    fn an_unread_filter_admits_nothing() {
        let r = Retention::new(vec![None], 0);
        assert!(!r.admit(&row(1)));
        assert_eq!(r.over(), [false]);
    }
}
