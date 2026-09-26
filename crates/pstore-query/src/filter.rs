//! Filters — which documents a query may answer with (M9b).
//!
//! ⚠️ **Evaluated before a leg's limit, never after it.** A predicate applied to a top-`k`
//! returns fewer than `k` whenever it is selective, and nothing says so. `run` therefore
//! builds each segment's [`Mask`] from its blocks and applies it to exhaustive legs.

use pstore_format::Value;
use std::collections::BTreeMap;

/// The document id, addressed as if it were an attribute — turbopuffer's `id`.
pub const ID: &str = "id";

/// A comparison's operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Present and equal.
    Eq,
    /// Present and less than.
    Lt,
    /// Present and at most.
    Lte,
    /// Present and greater than.
    Gt,
    /// Present and at least.
    Gte,
}

/// A predicate over one document's id and attributes.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// `attr op value`. A type mismatch or an absent attribute is false.
    Cmp(String, Op, Value),
    /// The attribute is absent — turbopuffer's `Eq null`.
    Absent(String),
    /// Present and equal to one of these.
    In(String, Vec<Value>),
    /// Every clause; `And []` is true.
    And(Vec<Predicate>),
    /// Any clause; `Or []` is false.
    Or(Vec<Predicate>),
    /// Not the clause. `NotEq v` is `Not(Cmp(Eq, v))`, so it admits an absent attribute.
    Not(Box<Predicate>),
}

impl Predicate {
    /// Whether a document with this id and these attributes is admitted.
    #[must_use]
    pub fn admits(&self, id: &str, attrs: &BTreeMap<String, Value>) -> bool {
        let get = |name: &str| -> Option<Value> {
            if name == ID {
                Some(Value::Str(id.to_owned()))
            } else {
                attrs.get(name).cloned()
            }
        };
        match self {
            Self::Cmp(name, op, want) => get(name).is_some_and(|have| compare(&have, *op, want)),
            Self::Absent(name) => get(name).is_none(),
            Self::In(name, set) => get(name).is_some_and(|have| set.contains(&have)),
            Self::And(all) => all.iter().all(|p| p.admits(id, attrs)),
            Self::Or(any) => any.iter().any(|p| p.admits(id, attrs)),
            Self::Not(p) => !p.admits(id, attrs),
        }
    }

    /// Whether a block whose integer attributes span these zones **could** hold an admitted
    /// row. `false` only when it provably cannot: a wrongly dropped block silently loses rows.
    ///
    /// ⚠️ A zone covers only rows that HAVE the attribute, and a block may hold rows without
    /// it — so absence, `Not`, and anything over a string or the id cannot prune.
    #[must_use]
    pub fn could_admit(&self, zones: &BTreeMap<String, (i64, i64)>) -> bool {
        match self {
            Self::Cmp(name, op, Value::Int(n)) => zones.get(name).is_none_or(|(lo, hi)| match op {
                Op::Eq => lo <= n && n <= hi,
                Op::Lt => lo < n,
                Op::Lte => lo <= n,
                Op::Gt => hi > n,
                Op::Gte => hi >= n,
            }),
            Self::In(name, set) => match zones.get(name) {
                // ⚠️ A string member can never prune: one name may hold strings in some rows
                // and integers in others (M9a stores values as written), and the zone covers
                // only the integers.
                Some((lo, hi)) => set.iter().any(|v| match v {
                    Value::Int(n) => lo <= n && n <= hi,
                    Value::Str(_) => true,
                }),
                None => true,
            },
            Self::And(all) => all.iter().all(|p| p.could_admit(zones)),
            Self::Or(any) => any.iter().any(|p| p.could_admit(zones)),
            Self::Cmp(..) | Self::Absent(_) | Self::Not(_) => true,
        }
    }
}

/// `have op want`: integers numerically, strings by bytes, anything else false.
fn compare(have: &Value, op: Op, want: &Value) -> bool {
    let ord = match (have, want) {
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Str(a), Value::Str(b)) => a.as_bytes().cmp(b.as_bytes()),
        _ => return false,
    };
    match op {
        Op::Eq => ord.is_eq(),
        Op::Lt => ord.is_lt(),
        Op::Lte => ord.is_le(),
        Op::Gt => ord.is_gt(),
        Op::Gte => ord.is_ge(),
    }
}

/// The data rows of one segment a predicate admits.
pub(crate) type Mask = std::collections::HashSet<usize>;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn zones(pairs: &[(&str, i64, i64)]) -> BTreeMap<String, (i64, i64)> {
        pairs
            .iter()
            .map(|(k, lo, hi)| ((*k).to_owned(), (*lo, *hi)))
            .collect()
    }

    fn eq(name: &str, n: i64) -> Predicate {
        Predicate::Cmp(name.to_owned(), Op::Eq, Value::Int(n))
    }

    #[test]
    fn a_not_never_prunes_because_a_zone_says_nothing_of_rows_without_the_attribute() {
        // A block holding `{a: 5}` and `{}` has the zone `a: (5, 5)`. `NotEq a 5` admits the
        // `{}` row, so pruning it by negating "cannot match" would lose that row.
        let z = zones(&[("a", 5, 5)]);
        assert!(!eq("a", 5).admits("x", &BTreeMap::new()) && !eq("a", 6).could_admit(&z));
        assert!(Predicate::Not(Box::new(eq("a", 5))).could_admit(&z));
        assert!(Predicate::Absent("a".to_owned()).could_admit(&z));
    }

    #[test]
    fn integer_comparisons_prune_at_their_exact_edges() {
        let z = zones(&[("a", 10, 20)]);
        let at = |op, n| Predicate::Cmp("a".to_owned(), op, Value::Int(n)).could_admit(&z);
        assert!(at(Op::Eq, 10) && at(Op::Eq, 20) && !at(Op::Eq, 9) && !at(Op::Eq, 21));
        assert!(at(Op::Lt, 11) && !at(Op::Lt, 10));
        assert!(at(Op::Lte, 10) && !at(Op::Lte, 9));
        assert!(at(Op::Gt, 19) && !at(Op::Gt, 20));
        assert!(at(Op::Gte, 20) && !at(Op::Gte, 21));
        let set = |vs: Vec<Value>| Predicate::In("a".to_owned(), vs).could_admit(&z);
        assert!(set(vec![Value::Int(3), Value::Int(15)]) && !set(vec![Value::Int(3)]));
        // A string member cannot prune: the zone covers only the name's integers.
        assert!(set(vec![Value::Str("x".to_owned())]));
    }

    #[test]
    fn and_prunes_if_any_child_does_or_only_if_all_do() {
        let z = zones(&[("a", 10, 20)]);
        let (yes, no) = (eq("a", 15), eq("a", 99));
        assert!(!Predicate::And(vec![yes.clone(), no.clone()]).could_admit(&z));
        assert!(Predicate::Or(vec![yes.clone(), no.clone()]).could_admit(&z));
        assert!(!Predicate::Or(vec![no.clone(), no]).could_admit(&z));
        assert!(Predicate::And(vec![]).could_admit(&z));
        assert!(!Predicate::Or(vec![]).could_admit(&z));
    }

    #[test]
    fn a_missing_zone_never_prunes() {
        // A zone-free segment (M9a's fallback) has no zones at all.
        assert!(eq("a", 99).could_admit(&BTreeMap::new()));
        assert!(Predicate::In("a".to_owned(), vec![Value::Int(1)]).could_admit(&BTreeMap::new()));
    }

    #[test]
    fn a_cross_type_comparison_is_false_not_ordered() {
        // `Value` orders `Int` before `Str`; comparing through that would make every integer
        // "less than" every string.
        let a = BTreeMap::from([("a".to_owned(), Value::Int(1))]);
        let lt = Predicate::Cmp("a".to_owned(), Op::Lt, Value::Str("5".to_owned()));
        assert!(!lt.admits("x", &a));
        let gt = Predicate::Cmp("a".to_owned(), Op::Gt, Value::Str("5".to_owned()));
        assert!(!gt.admits("x", &a));
    }
}
