//! Aggregations (M12): counts and sums over the rows a filter admits, optionally grouped.
//!
//! ⚠️ **Bounded memory, exact answers.** Only the `top_k` smallest group keys are held. A key
//! larger than every kept key when the aggregator is full is dropped, and since the largest
//! kept key only falls once full, a dropped key never returns: every kept key's aggregate is
//! complete. Per segment each aggregator keeps its own `top_k` smallest and they are merged;
//! a key among the global `top_k` smallest is among every segment's own, so the merge is exact.

use crate::filter::Predicate;
use crate::order::visit;
use crate::run::{QueryError, Target};
use pstore_blob::BlobStore;
use pstore_format::{Document, Number, Value, cmp_numbers};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

/// One aggregate over the admitted rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Aggregate {
    /// Rows; with an attribute, the rows where it is present.
    Count(Option<String>),
    /// The numeric values of an attribute. Anything not a number adds nothing.
    Sum(String),
}

/// What to compute: labelled aggregates, the attributes to group by, and how many groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// Each aggregate with the label it is reported under, in request order.
    pub labels: Vec<(String, Aggregate)>,
    /// Attributes whose values key a group; empty for one group over everything.
    pub group_by: Vec<String>,
    /// The most groups kept: the smallest keys.
    pub top_k: usize,
}

impl Spec {
    /// Whether every aggregate is a count of rows and nothing is grouped: the shape HEAD alone
    /// can answer when nothing is filtered and nothing is unfolded (M12's fast path).
    #[must_use]
    pub fn counts_rows_only(&self) -> bool {
        self.group_by.is_empty()
            && self
                .labels
                .iter()
                .all(|(_, a)| matches!(a, Aggregate::Count(None)))
    }
}

/// A group key component: a stored value, normalized so that the filters' equality is the
/// key's -- `1` and `1.0` are one key -- and totally ordered as a `rank_by` ascending orders
/// scalars, arrays element by element, and absent last.
#[derive(Debug, Clone)]
pub enum Key {
    /// A bool.
    Bool(bool),
    /// A number: an integral float within `i64` is held as the integer.
    Num(Number),
    /// A datetime, by instant. Never a number.
    DateTime(i64),
    /// A string, compared bytewise.
    Str(String),
    /// A UUID, by its bytes (M171).
    Uuid([u8; 16]),
    /// Bytes, bytewise (M171).
    Bytes(Vec<u8>),
    /// An array, lexicographically, a proper prefix first.
    Array(Vec<Key>),
    /// No value.
    Absent,
}

impl Key {
    /// A value's key, `None` being [`Key::Absent`] (public since M146, whose group limit keys
    /// rows as `group_by` does).
    #[must_use]
    pub fn of(v: Option<&Value>) -> Self {
        match v {
            None => Self::Absent,
            Some(Value::Bool(b)) => Self::Bool(*b),
            Some(Value::Int(n)) => Self::Num(Number::Int(*n)),
            Some(Value::Float(f)) => Self::Num(normalized(*f)),
            Some(Value::Uint(n)) => Self::Num(unsigned(*n)),
            Some(Value::DateTime(t)) => Self::DateTime(*t),
            Some(Value::Str(s)) => Self::Str(s.clone()),
            Some(Value::Uuid(u)) => Self::Uuid(*u),
            Some(Value::Bytes(b)) => Self::Bytes(b.clone()),
            Some(Value::Array(a)) => Self::Array(a.iter().map(|v| Self::of(Some(v))).collect()),
        }
    }

    fn group(&self) -> u8 {
        match self {
            Self::Bool(_) => 0,
            Self::Num(_) => 1,
            Self::DateTime(_) => 2,
            Self::Str(_) => 3,
            Self::Uuid(_) => 4,
            Self::Bytes(_) => 5,
            Self::Array(_) => 6,
            Self::Absent => 7,
        }
    }

    /// The value the key reports, or `None` for absent.
    #[must_use]
    pub fn value(&self) -> Option<Value> {
        match self {
            Self::Bool(b) => Some(Value::Bool(*b)),
            Self::Num(Number::Int(n)) => Some(Value::Int(*n)),
            Self::Num(Number::Float(f)) => Some(Value::Float(*f)),
            Self::Num(Number::Uint(n)) => Some(Value::Uint(*n)),
            Self::DateTime(t) => Some(Value::DateTime(*t)),
            Self::Str(s) => Some(Value::Str(s.clone())),
            Self::Uuid(u) => Some(Value::Uuid(*u)),
            Self::Bytes(b) => Some(Value::Bytes(b.clone())),
            Self::Array(a) => Some(Value::Array(a.iter().filter_map(Self::value).collect())),
            Self::Absent => None,
        }
    }
}

/// An integral float within `i64` as the integer, exactly -- `-0.0` included; within `u64`
/// above that (M172), as the unsigned integer; any other float as itself. `i64::MAX as f64`
/// rounds up to 2^63 and `u64::MAX as f64` to 2^64, which are out of range, hence `<`.
fn normalized(f: f64) -> Number {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the bounds are the exact powers of two; the cast is exact inside them"
    )]
    if f.fract() != 0.0 || f < i64::MIN as f64 {
        Number::Float(f)
    } else if f < i64::MAX as f64 {
        Number::Int(f as i64)
    } else if f < u64::MAX as f64 {
        Number::Uint(f as u64)
    } else {
        Number::Float(f)
    }
}

/// A uint64 within `i64` as the int it equals (M172), so the two are reported alike.
fn unsigned(n: u64) -> Number {
    i64::try_from(n).map_or(Number::Uint(n), Number::Int)
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Bool(a), Self::Bool(b)) => a.cmp(b),
            // A stored float is finite (`check_storable`), so `None` is unreachable.
            (Self::Num(a), Self::Num(b)) => cmp_numbers(*a, *b).unwrap_or(Ordering::Equal),
            (Self::DateTime(a), Self::DateTime(b)) => a.cmp(b),
            (Self::Str(a), Self::Str(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Self::Uuid(a), Self::Uuid(b)) => a.cmp(b),
            (Self::Bytes(a), Self::Bytes(b)) => a.cmp(b),
            (Self::Array(a), Self::Array(b)) => a.cmp(b),
            _ => self.group().cmp(&other.group()),
        }
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Key {}

/// One aggregate's running state.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Acc {
    count: u64,
    ints: i128,
    floats: f64,
    any_float: bool,
    /// Whether a uint64 was summed (M172): only then may an integer sum answer above `i64`.
    any_uint: bool,
}

impl Acc {
    fn add(&mut self, other: &Self) {
        self.count += other.count;
        self.ints += other.ints;
        self.floats += other.floats;
        self.any_float |= other.any_float;
        self.any_uint |= other.any_uint;
    }
}

/// An aggregate's result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Total {
    /// A count, or a sum of integers only within `i64`.
    Int(i64),
    /// A sum of integers, a uint64 among them, above `i64` and within `u64` (M172).
    Uint(u64),
    /// A sum with a float in it, or of integers outside `i64`.
    Float(f64),
    /// A float sum that overflowed: JSON cannot carry ±∞.
    Null,
}

/// Adds one row to its group's accumulators.
fn fold(labels: &[(String, Aggregate)], accs: &mut [Acc], doc: &Document) {
    for ((_, agg), acc) in labels.iter().zip(accs.iter_mut()) {
        match agg {
            Aggregate::Count(None) => acc.count += 1,
            Aggregate::Count(Some(a)) => acc.count += u64::from(doc.attrs.contains_key(a)),
            Aggregate::Sum(a) => match doc.attrs.get(a) {
                Some(Value::Int(n)) => acc.ints += i128::from(*n),
                Some(Value::Uint(n)) => {
                    acc.ints += i128::from(*n);
                    acc.any_uint = true;
                }
                Some(Value::Float(f)) => {
                    acc.floats += f;
                    acc.any_float = true;
                }
                _ => {}
            },
        }
    }
}

/// Counts and sums, per group, over rows offered one at a time.
#[derive(Debug, Clone)]
pub struct Aggregator {
    spec: Arc<Spec>,
    groups: BTreeMap<Vec<Key>, Vec<Acc>>,
}

impl Aggregator {
    /// An aggregator computing `spec`.
    #[must_use]
    pub fn new(spec: Spec) -> Self {
        Self {
            spec: Arc::new(spec),
            groups: BTreeMap::new(),
        }
    }

    fn empty_like(&self) -> Self {
        Self {
            spec: Arc::clone(&self.spec),
            groups: BTreeMap::new(),
        }
    }

    /// Makes room for `key`, if it is among the `top_k` smallest: whether it may be updated.
    fn admit(&mut self, key: &[Key]) -> bool {
        if self.groups.contains_key(key) {
            return true;
        }
        if !self.make_room(key) {
            return false;
        }
        self.groups
            .insert(key.to_vec(), vec![Acc::default(); self.spec.labels.len()]);
        true
    }

    /// For a `key` not held: evicts the largest held if the groups are full and `key` is
    /// smaller. Whether `key` may be inserted.
    fn make_room(&mut self, key: &[Key]) -> bool {
        if self.groups.len() >= self.spec.top_k.max(1) {
            match self.groups.last_key_value() {
                Some((largest, _)) if key < largest.as_slice() => {
                    self.groups.pop_last();
                }
                _ => return false,
            }
        }
        true
    }

    /// Offers a row the filter admitted. Whether it was aggregated: `false` when its key is
    /// not among the `top_k` smallest.
    pub fn offer(&mut self, doc: &Document) -> bool {
        let key: Vec<Key> = self
            .spec
            .group_by
            .iter()
            .map(|a| Key::of(doc.attrs.get(a)))
            .collect();
        // ⚠️ An existing group found in one search, and a new one's key moved in rather than
        // copied (M119): `admit` then `get_mut` searched twice, and the spec's `Arc` was
        // cloned, for every row.
        if let Some(accs) = self.groups.get_mut(&key) {
            fold(&self.spec.labels, accs, doc);
            return true;
        }
        if !self.make_room(&key) {
            return false;
        }
        let mut accs = vec![Acc::default(); self.spec.labels.len()];
        fold(&self.spec.labels, &mut accs, doc);
        self.groups.insert(key, accs);
        true
    }

    /// Counts `rows` rows without offering them: HEAD's arithmetic, for the fast path. Only
    /// meaningful for [`Spec::counts_rows_only`].
    pub fn count_rows(&mut self, rows: u64) {
        if self.admit(&[])
            && let Some(accs) = self.groups.get_mut(&Vec::new())
        {
            for acc in accs {
                acc.count += rows;
            }
        }
    }

    /// Adds another aggregator's partials, keeping the `top_k` smallest keys.
    pub fn merge(&mut self, other: Self) {
        for (key, accs) in other.groups {
            if !self.admit(&key) {
                continue;
            }
            if let Some(mine) = self.groups.get_mut(&key) {
                for (m, o) in mine.iter_mut().zip(&accs) {
                    m.add(o);
                }
            }
        }
    }

    /// How many groups it holds: what the memory bound is asserted on.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.groups.len()
    }

    /// Each group's key and its totals, in label order, ascending by key. With no `group_by`
    /// there is exactly one group, with an empty key, even over no rows.
    #[must_use]
    pub fn finish(mut self) -> Vec<(Vec<Key>, Vec<Total>)> {
        if self.spec.group_by.is_empty() && self.groups.is_empty() {
            self.groups
                .insert(Vec::new(), vec![Acc::default(); self.spec.labels.len()]);
        }
        let spec = Arc::clone(&self.spec);
        self.groups
            .into_iter()
            .map(|(key, accs)| {
                let totals = spec
                    .labels
                    .iter()
                    .zip(accs)
                    .map(|((_, agg), acc)| total(agg, acc))
                    .collect();
                (key, totals)
            })
            .collect()
    }
}

/// An integer sum as an int, or as a uint64 when one was summed (M172), if it fits.
fn integral(sum: i128, any_uint: bool) -> Option<Total> {
    match i64::try_from(sum) {
        Ok(n) => Some(Total::Int(n)),
        Err(_) if any_uint => u64::try_from(sum).ok().map(Total::Uint),
        Err(_) => None,
    }
}

fn total(agg: &Aggregate, acc: Acc) -> Total {
    match agg {
        Aggregate::Count(_) => Total::Int(i64::try_from(acc.count).unwrap_or(i64::MAX)),
        Aggregate::Sum(_) => {
            #[allow(
                clippy::cast_precision_loss,
                reason = "stated: an integer sum outside i64, or one beside a float, is an f64"
            )]
            let ints = acc.ints as f64;
            if acc.any_float {
                let sum = acc.floats + ints;
                if sum.is_finite() {
                    Total::Float(sum)
                } else {
                    Total::Null
                }
            } else {
                integral(acc.ints, acc.any_uint).unwrap_or(Total::Float(ints))
            }
        }
    }
}

/// Every row of `targets` that `filter` admits -- less deleted and shadowed rows, as
/// [`crate::select`] visits them -- offered to `aggregator`, one aggregator per segment merged.
///
/// # Errors
/// A segment, delete vector or block that cannot be read or decoded.
pub async fn aggregate<S: BlobStore>(
    store: &S,
    targets: &[Target],
    filter: Option<&Predicate>,
    shadow: &HashSet<String>,
    aggregator: &mut Aggregator,
) -> Result<(), QueryError> {
    let blank = aggregator.empty_like();
    let parts = visit(
        store,
        targets,
        filter,
        shadow,
        || blank.empty_like(),
        |a: &mut Aggregator, doc: Document| {
            a.offer(&doc);
        },
    )
    .await?;
    for part in parts {
        aggregator.merge(part);
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::float_cmp,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn doc(id: &str, k: Value) -> Document {
        let mut d = Document::new(id, vec![1.0]);
        d.attrs.insert("k".to_owned(), k);
        d
    }

    fn by_k(top_k: usize) -> Aggregator {
        Aggregator::new(Spec {
            labels: vec![("n".to_owned(), Aggregate::Count(None))],
            group_by: vec!["k".to_owned()],
            top_k,
        })
    }

    #[test]
    fn it_holds_at_most_top_k_groups_and_keeps_the_smallest_exactly() {
        let mut a = by_k(3);
        // 1,000 distinct keys in a scrambled order, each offered twice.
        for round in 0..2 {
            for i in 0..1000_i64 {
                let k = (i * 7919) % 1000;
                a.offer(&doc(&format!("{round}-{i}"), Value::Int(k)));
                assert!(a.len() <= 3, "held {}", a.len());
            }
        }
        let got = a.finish();
        let keys: Vec<_> = got.iter().map(|(k, _)| k[0].value()).collect();
        assert_eq!(
            keys,
            [
                Some(Value::Int(0)),
                Some(Value::Int(1)),
                Some(Value::Int(2))
            ]
        );
        assert!(got.iter().all(|(_, t)| t[0] == Total::Int(2)));
    }

    #[test]
    fn a_merge_is_exact_when_a_global_key_is_a_segments_last_kept() {
        let mut one = by_k(3);
        for k in ["a", "b", "c", "x"] {
            one.offer(&doc(k, Value::Str(k.to_owned())));
        }
        let mut two = by_k(3);
        for k in ["c", "c", "d"] {
            two.offer(&doc(k, Value::Str(k.to_owned())));
        }
        let mut all = by_k(3);
        all.merge(one);
        all.merge(two);
        let got: Vec<_> = all
            .finish()
            .into_iter()
            .map(|(k, t)| (k[0].value().unwrap(), t[0]))
            .collect();
        assert_eq!(
            got,
            [
                (Value::Str("a".into()), Total::Int(1)),
                (Value::Str("b".into()), Total::Int(1)),
                (Value::Str("c".into()), Total::Int(3)),
            ]
        );
    }

    #[test]
    fn keys_order_and_unify_as_stated() {
        let keys = [
            Key::of(None),
            Key::of(Some(&Value::Array(vec![Value::Int(1), Value::Int(2)]))),
            Key::of(Some(&Value::Array(vec![Value::Float(1.0)]))),
            // M171: UUIDs then bytes, after strings and before arrays.
            Key::of(Some(&Value::Bytes(vec![2]))),
            Key::of(Some(&Value::Bytes(vec![1, 9]))),
            Key::of(Some(&Value::Uuid([2; 16]))),
            Key::of(Some(&Value::Uuid([1; 16]))),
            Key::of(Some(&Value::Str("b".into()))),
            Key::of(Some(&Value::Str("a".into()))),
            Key::of(Some(&Value::DateTime(5))),
            Key::of(Some(&Value::Float(2.5))),
            Key::of(Some(&Value::Int(1))),
            Key::of(Some(&Value::Bool(true))),
            Key::of(Some(&Value::Bool(false))),
        ];
        let mut sorted = keys.to_vec();
        sorted.sort();
        let reversed: Vec<_> = keys.iter().rev().cloned().collect();
        assert_eq!(sorted, reversed);
        assert_eq!(
            Key::of(Some(&Value::Int(1))),
            Key::of(Some(&Value::Float(1.0)))
        );
        assert_eq!(
            Key::of(Some(&Value::Int(0))),
            Key::of(Some(&Value::Float(-0.0)))
        );
        assert_eq!(
            Key::of(Some(&Value::Array(vec![Value::Int(1)]))),
            Key::of(Some(&Value::Array(vec![Value::Float(1.0)])))
        );
        assert_ne!(
            Key::of(Some(&Value::Int(5))),
            Key::of(Some(&Value::DateTime(5)))
        );
        // Within a group, by value: two datetimes, bools, strings, UUIDs or bytes are not one key.
        for (lo, hi) in [
            (Value::DateTime(1), Value::DateTime(5)),
            (Value::Bool(false), Value::Bool(true)),
            (Value::Str("a".into()), Value::Str("b".into())),
            (Value::Int(1), Value::Float(1.5)),
            (Value::Uuid([1; 16]), Value::Uuid([2; 16])),
            (Value::Bytes(vec![1, 9]), Value::Bytes(vec![2])),
        ] {
            assert!(Key::of(Some(&lo)) < Key::of(Some(&hi)), "{lo:?} < {hi:?}");
        }
        // M171: a key reports the value it holds.
        for v in [Value::Uuid([3; 16]), Value::Bytes(vec![0, 255])] {
            assert_eq!(Key::of(Some(&v)).value(), Some(v));
        }
        // 2^60 as a float is integral and inside i64: reported as the integer, exactly.
        let big = 2f64.powi(60);
        assert_eq!(
            Key::of(Some(&Value::Float(big))).value(),
            Some(Value::Int(1 << 60))
        );
        // 2^63 is not inside i64, but is inside u64 (M172): reported as that uint64, and one
        // key with it. 2^64 is inside neither.
        let edge = 2f64.powi(63);
        assert_eq!(
            Key::of(Some(&Value::Float(edge))).value(),
            Some(Value::Uint(1 << 63))
        );
        let past = 2f64.powi(64);
        assert_eq!(
            Key::of(Some(&Value::Float(past))).value(),
            Some(Value::Float(past))
        );
        // -2^63 is inside i64 and reported as the integer; below it, a float stays one.
        let low = -(2f64.powi(63));
        assert_eq!(
            Key::of(Some(&Value::Float(low))).value(),
            Some(Value::Int(i64::MIN))
        );
        assert_eq!(
            Key::of(Some(&Value::Float(-1e19))).value(),
            Some(Value::Float(-1e19))
        );
        // M172: a uint64 within i64 reports as the int it equals; above, as itself.
        assert_eq!(Key::of(Some(&Value::Uint(5))).value(), Some(Value::Int(5)));
        assert_eq!(
            Key::of(Some(&Value::Uint(u64::MAX))).value(),
            Some(Value::Uint(u64::MAX))
        );
        assert_eq!(
            Key::of(Some(&Value::Uint(1 << 63))),
            Key::of(Some(&Value::Float(edge)))
        );
        assert!(Key::of(Some(&Value::Int(i64::MAX))) < Key::of(Some(&Value::Uint(1 << 63))));
        assert!(Key::of(Some(&Value::Uint(u64::MAX))) < Key::of(Some(&Value::Float(past))));
    }

    #[test]
    fn sums_are_typed_as_stated() {
        let spec = |a| Spec {
            labels: vec![("s".to_owned(), a)],
            group_by: vec![],
            top_k: 1,
        };
        let sum = |vals: &[Value]| {
            let mut a = Aggregator::new(spec(Aggregate::Sum("k".into())));
            for (i, v) in vals.iter().enumerate() {
                a.offer(&doc(&i.to_string(), v.clone()));
            }
            a.finish()[0].1[0]
        };
        assert_eq!(sum(&[]), Total::Int(0));
        assert_eq!(
            sum(&[Value::Int(2), Value::Str("x".into()), Value::Int(3)]),
            Total::Int(5)
        );
        assert_eq!(sum(&[Value::Int(2), Value::Float(0.5)]), Total::Float(2.5));
        assert_eq!(
            sum(&[Value::Int(i64::MAX), Value::Int(1)]),
            Total::Float(i64::MAX as f64 + 1.0)
        );
        assert_eq!(
            sum(&[Value::Float(f64::MAX), Value::Float(f64::MAX)]),
            Total::Null
        );
        assert_eq!(sum(&[Value::Int(i64::MAX)]), Total::Int(i64::MAX));
        // M172: with a uint64 in it, an integer sum is exact within u64, as well as i64.
        assert_eq!(sum(&[Value::Uint(u64::MAX)]), Total::Uint(u64::MAX));
        assert_eq!(
            sum(&[Value::Uint(u64::MAX), Value::Int(-1)]),
            Total::Uint(u64::MAX - 1)
        );
        assert_eq!(sum(&[Value::Uint(3), Value::Int(-5)]), Total::Int(-2));
        assert_eq!(
            sum(&[Value::Uint(u64::MAX), Value::Uint(1)]),
            Total::Float(u64::MAX as f64 + 1.0)
        );
        assert_eq!(sum(&[Value::Uint(1), Value::Float(0.5)]), Total::Float(1.5));
        // Merged partial sums keep whether a uint64 was summed.
        let mut left = Aggregator::new(spec(Aggregate::Sum("k".into())));
        left.offer(&doc("x", Value::Uint(u64::MAX)));
        let mut right = Aggregator::new(spec(Aggregate::Sum("k".into())));
        right.offer(&doc("y", Value::Int(-2)));
        right.merge(left);
        assert_eq!(right.finish()[0].1[0], Total::Uint(u64::MAX - 2));
        let count = |a: Aggregate| {
            let mut g = Aggregator::new(spec(a));
            g.offer(&doc("x", Value::Int(1)));
            g.offer(&Document::new("y", vec![1.0]));
            g.finish()[0].1[0]
        };
        assert_eq!(count(Aggregate::Count(None)), Total::Int(2));
        assert_eq!(count(Aggregate::Count(Some("k".into()))), Total::Int(1));
        let mut g = Aggregator::new(spec(Aggregate::Count(None)));
        g.count_rows(7);
        g.offer(&doc("z", Value::Int(1)));
        assert_eq!(g.finish()[0].1[0], Total::Int(8));
    }
}
