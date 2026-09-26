//! Order by attribute (M9e): every admitted row of every segment, ranked by one attribute and
//! **selected** -- never sorted whole -- into the `offset + limit` the caller asked for.

use crate::filter::Predicate;
use crate::run::{QueryError, Target};
use pstore_blob::BlobStore;
use pstore_format::{Document, Segment, Value};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};

/// What to order by: an attribute name, or `id`, and the direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    /// The attribute, or [`crate::ID_ATTRIBUTE`] for the document id.
    pub attr: String,
    /// Descending when true.
    pub desc: bool,
}

/// A row's place in the order, computed once.
///
/// ⚠️ **Not `Value`'s own order reversed for `desc`.** The absent group sorts last in BOTH
/// directions, and ties break on the id ascending in both: `asc` is integers, strings, absent;
/// `desc` is strings, integers, absent, each group's values reversed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rank {
    desc: bool,
    group: u8,
    value: Option<Value>,
    id: String,
}

impl Rank {
    fn of(by: &OrderBy, doc: &Document) -> Self {
        let value = if by.attr == crate::ID_ATTRIBUTE {
            Some(Value::Str(doc.id.clone()))
        } else {
            doc.attrs.get(&by.attr).cloned()
        };
        let group = match (&value, by.desc) {
            (Some(Value::Int(_)), false) | (Some(Value::Str(_)), true) => 0,
            (Some(_), _) => 1,
            (None, _) => 2,
        };
        Self {
            desc: by.desc,
            group,
            value,
            id: doc.id.clone(),
        }
    }
}

impl Ord for Rank {
    fn cmp(&self, other: &Self) -> Ordering {
        let values = match (&self.value, &other.value) {
            (Some(Value::Int(a)), Some(Value::Int(b))) => a.cmp(b),
            // Bytewise, as the `Gt` cursor compares (`filter.rs`).
            (Some(Value::Str(a)), Some(Value::Str(b))) => a.as_bytes().cmp(b.as_bytes()),
            // Different groups never reach here; the group decided.
            _ => Ordering::Equal,
        };
        self.group
            .cmp(&other.group)
            .then(if self.desc { values.reverse() } else { values })
            .then_with(|| self.id.as_bytes().cmp(other.id.as_bytes()))
    }
}

impl PartialOrd for Rank {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A row and its rank, ordered by the rank alone.
#[derive(Debug)]
struct Ranked(Rank, Document);

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for Ranked {}
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The first `cap` rows in `by`'s order, from rows offered one at a time.
///
/// ⚠️ **Bounded, and that is the point** (spec review of M9e): a sort holds every row it is
/// given, and an unfiltered order-by is given the whole index. This holds at most `cap`.
#[derive(Debug)]
pub struct Selector {
    by: OrderBy,
    cap: usize,
    /// A max-heap: its top is the worst row kept, the one a better row evicts.
    kept: BinaryHeap<Ranked>,
}

impl Selector {
    /// A selector keeping the first `cap` rows.
    #[must_use]
    pub fn new(by: OrderBy, cap: usize) -> Self {
        Self {
            by,
            cap,
            kept: BinaryHeap::new(),
        }
    }

    /// Offers a row. Among rows of equal rank -- only possible for one id offered twice -- the
    /// first offered is kept.
    pub fn offer(&mut self, doc: Document) {
        if self.cap == 0 {
            return;
        }
        let rank = Rank::of(&self.by, &doc);
        if self.kept.len() == self.cap {
            match self.kept.peek() {
                Some(worst) if rank < worst.0 => {
                    self.kept.pop();
                }
                _ => return,
            }
        }
        self.kept.push(Ranked(rank, doc));
    }

    /// How many rows it holds: what the bound is asserted on.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.kept.len()
    }

    /// The rows kept, in order.
    #[must_use]
    pub fn into_sorted(self) -> Vec<Document> {
        self.kept
            .into_sorted_vec()
            .into_iter()
            .map(|Ranked(_, d)| d)
            .collect()
    }
}

/// Every row of `targets` that `filter` admits, less each segment's deleted rows and -- in a
/// shadowed segment -- the ids in `shadow`, offered to `selector`.
///
/// **Two rounds:** each segment is opened with its delete vector (one fan-out), then each
/// segment's admitted blocks are fetched in one coalesced plan (a second) and decoded block by
/// block into the selector. No row round follows: the blocks carry ids and attributes.
///
/// # Errors
/// A segment, delete vector or block that cannot be read or decoded.
pub async fn select<S: BlobStore>(
    store: &S,
    targets: &[Target],
    filter: Option<&Predicate>,
    shadow: &HashSet<String>,
    selector: &mut Selector,
) -> Result<(), QueryError> {
    let opened = futures_util::future::try_join_all(targets.iter().map(|t| async move {
        let (segment, deleted) =
            futures_util::future::join(Segment::open(store, &t.segment), async {
                match &t.deleted {
                    // As the ranked query reads it: an unreadable vector is an error, never
                    // an answer that includes the rows it deletes.
                    Some(k) => store
                        .get_immutable(k, pstore_blob::Class::Pinned)
                        .await
                        .map(|raw| crate::deletes::decode(&raw))
                        .map_err(|e| QueryError::Format(pstore_format::FormatError::from(e))),
                    None => Ok(HashSet::new()),
                }
            })
            .await;
        Ok::<_, QueryError>((segment?, deleted?))
    }))
    .await?;
    let (by, cap) = (&selector.by, selector.cap);
    // ⚠️ **A selector per segment**, merged after: collecting a segment's admitted rows and
    // selecting afterwards would hold the whole segment decoded, which is the sort this exists
    // not to do.
    let per_segment = futures_util::future::try_join_all(opened.iter().zip(targets).map(
        |((segment, deleted), t)| async move {
            let mut local = Selector::new(by.clone(), cap);
            segment
                .visit_rows_where(
                    store,
                    &t.segment,
                    |zones| filter.is_none_or(|f| f.could_admit(zones)),
                    |row, doc| {
                        let hidden =
                            deleted.contains(&row) || (t.shadowed && shadow.contains(&doc.id));
                        if !hidden && filter.is_none_or(|f| f.admits(&doc.id, &doc.attrs)) {
                            local.offer(doc);
                        }
                    },
                )
                .await?;
            Ok::<_, QueryError>(local.into_sorted())
        },
    ))
    .await?;
    for doc in per_segment.into_iter().flatten() {
        selector.offer(doc);
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn doc(id: &str, v: Option<Value>) -> Document {
        let mut d = Document::new(id, vec![1.0]);
        if let Some(v) = v {
            d.attrs.insert("k".to_owned(), v);
        }
        d
    }

    /// A pseudo-random mix of integers, strings, and absent values, with ties.
    fn mixed(n: usize) -> Vec<Document> {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        (0..n)
            .map(|i| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let v = match state % 4 {
                    0 => None,
                    1 => Some(Value::Int((state >> 8) as i64 % 7 - 3)),
                    2 => Some(Value::Str(
                        ["b", "B", "a", "ab", ""][(state >> 8) as usize % 5].to_owned(),
                    )),
                    _ => Some(Value::Int((state >> 16) as i64 % 100)),
                };
                doc(
                    &format!("d{:03}", (state >> 24) % 1000 + i as u64 * 1000),
                    v,
                )
            })
            .collect()
    }

    fn by(desc: bool) -> OrderBy {
        OrderBy {
            attr: "k".to_owned(),
            desc,
        }
    }

    #[test]
    fn the_order_is_the_one_written_down() {
        let docs = vec![
            doc("a", Some(Value::Int(3))),
            doc("b", Some(Value::Str("pear".to_owned()))),
            doc("c", None),
            doc("d", Some(Value::Int(-7))),
            doc("e", Some(Value::Str("apple".to_owned()))),
            doc("f", Some(Value::Int(3))),
            doc("g", None),
            doc("h", Some(Value::Str("Zebra".to_owned()))),
        ];
        for (desc, want) in [
            (false, ["d", "a", "f", "h", "e", "b", "c", "g"]),
            (true, ["b", "e", "h", "a", "f", "d", "c", "g"]),
        ] {
            let mut s = Selector::new(by(desc), 100);
            for d in docs.iter().rev().cloned() {
                s.offer(d);
            }
            let got: Vec<String> = s.into_sorted().into_iter().map(|d| d.id).collect();
            assert_eq!(got, want, "desc {desc}");
        }
    }

    #[test]
    fn the_selector_holds_at_most_its_cap_and_equals_a_sort_truncated() {
        let docs = mixed(500);
        for desc in [false, true] {
            let mut sorted = docs.clone();
            sorted.sort_by(|a, b| Rank::of(&by(desc), a).cmp(&Rank::of(&by(desc), b)));
            for cap in [0, 1, 7, 64, 499, 500, 600] {
                let mut s = Selector::new(by(desc), cap);
                for d in docs.iter().cloned() {
                    s.offer(d);
                    assert!(s.len() <= cap, "cap {cap} exceeded");
                }
                let got: Vec<String> = s.into_sorted().into_iter().map(|d| d.id).collect();
                let want: Vec<String> = sorted.iter().take(cap).map(|d| d.id.clone()).collect();
                assert_eq!(got, want, "desc {desc}, cap {cap}");
            }
        }
    }

    #[test]
    fn the_selector_holds_exactly_what_it_was_offered_up_to_its_cap() {
        for cap in [0, 3, 10] {
            let mut s = Selector::new(by(false), cap);
            for (n, d) in mixed(6).into_iter().enumerate() {
                s.offer(d);
                assert_eq!(s.len(), (n + 1).min(cap));
            }
        }
    }

    #[test]
    fn among_equal_ranks_the_first_offered_is_kept() {
        let tagged = |tag: i64| {
            let mut d = doc("a", Some(Value::Int(1)));
            d.attrs.insert("tag".to_owned(), Value::Int(tag));
            d
        };
        let mut s = Selector::new(by(false), 1);
        s.offer(tagged(1));
        s.offer(tagged(2));
        let kept = s.into_sorted();
        assert_eq!(kept[0].attrs["tag"], Value::Int(1));
    }

    #[test]
    fn a_ranked_rows_equality_is_its_ranks() {
        let a = Ranked(
            Rank::of(&by(false), &doc("a", Some(Value::Int(1)))),
            doc("a", None),
        );
        let same = Ranked(
            Rank::of(&by(false), &doc("a", Some(Value::Int(1)))),
            doc("z", None),
        );
        let other = Ranked(
            Rank::of(&by(false), &doc("b", Some(Value::Int(1)))),
            doc("a", None),
        );
        assert!(
            a == same,
            "equal ranks are equal rows, whatever the document"
        );
        assert!(a != other);
    }

    #[test]
    fn ordering_by_id_is_bytewise() {
        let mut s = Selector::new(
            OrderBy {
                attr: crate::ID_ATTRIBUTE.to_owned(),
                desc: false,
            },
            10,
        );
        for id in ["b", "B", "a", "ab"] {
            s.offer(doc(id, None));
        }
        let got: Vec<String> = s.into_sorted().into_iter().map(|d| d.id).collect();
        assert_eq!(got, ["B", "a", "ab", "b"]);
    }
}
