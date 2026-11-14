//! Per-block aggregate statistics (M232): for each numeric attribute, how many rows of a block
//! hold it and their sum, so an aggregation over a block its filter admits whole reads three
//! numbers from the open round instead of the block.
//!
//! ⚠️ **Only an attribute every value of which, in the segment, is one numeric kind**: all
//! ints, or all floats. Then a row holds the name exactly when it holds a number of that kind,
//! so the count is both `Count(name)`'s and `Sum`'s, and the block's zone map is its `Min` and
//! `Max`. A name that is ever a string, an array, a uint64 or a mixture is left out and
//! aggregates as before, by reading the rows.
//!
//! ⚠️ An int sum is exact or absent: an attribute whose block sum leaves `i64` is left out.
//! A float sum is the block's rows summed in row order, so a total built from block sums can
//! differ from one built row by row in the last bits, as two segments' partials already do.

use crate::{Document, FormatError, Value};
use std::collections::BTreeMap;

/// What an attribute's values are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Every value is a [`Value::Int`].
    Int,
    /// Every value is a [`Value::Float`].
    Float,
}

/// One block's statistics for one attribute.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stat {
    /// An int attribute.
    Int {
        /// Rows holding it.
        count: u32,
        /// Their exact sum.
        sum: i64,
    },
    /// A float attribute.
    Float {
        /// Rows holding it.
        count: u32,
        /// Their sum in row order.
        sum: f64,
    },
}

impl Stat {
    /// The rows of the block holding the attribute.
    #[must_use]
    pub fn count(self) -> u32 {
        match self {
            Self::Int { count, .. } | Self::Float { count, .. } => count,
        }
    }
}

/// Every block's statistics for the attributes the writer kept.
#[derive(Debug, Clone, PartialEq)]
pub struct Aggregates {
    blocks: usize,
    /// Per attribute: its kind, then per block a count and an 8-byte sum.
    attrs: BTreeMap<String, (Kind, Vec<(u32, u64)>)>,
}

/// Bytes an attribute costs beside its name: a kind byte, then a count and a sum a block.
fn attr_len(name: &str, blocks: usize) -> usize {
    4 + name.len() + 1 + blocks * 12
}

impl Aggregates {
    /// The statistics of `chunks`' numeric attributes that fit `spare` bytes, names in order,
    /// or `None` when none does.
    #[must_use]
    pub fn build(chunks: &[&[Document]], spare: usize) -> Option<Self> {
        let mut kinds: BTreeMap<&str, Option<Kind>> = BTreeMap::new();
        for d in chunks.iter().flat_map(|c| c.iter()) {
            for (k, v) in &d.attrs {
                let kind = match v {
                    Value::Int(_) => Some(Kind::Int),
                    Value::Float(_) => Some(Kind::Float),
                    _ => None,
                };
                let e = kinds.entry(k.as_str()).or_insert(kind);
                if *e != kind {
                    *e = None;
                }
            }
        }
        let mut attrs = BTreeMap::new();
        for (name, kind) in kinds {
            let Some(kind) = kind else { continue };
            let mut per = Vec::with_capacity(chunks.len());
            let mut exact = true;
            for c in chunks {
                let mut count = 0u32;
                let (mut ints, mut floats) = (0i128, 0f64);
                for d in *c {
                    match d.attrs.get(name) {
                        Some(Value::Int(n)) => {
                            count += 1;
                            ints += i128::from(*n);
                        }
                        Some(Value::Float(f)) => {
                            count += 1;
                            floats += f;
                        }
                        _ => {}
                    }
                }
                let sum = match kind {
                    Kind::Int => match i64::try_from(ints) {
                        Ok(s) => s as u64,
                        Err(_) => {
                            exact = false;
                            0
                        }
                    },
                    Kind::Float if floats.is_finite() => floats.to_bits(),
                    Kind::Float => {
                        exact = false;
                        0
                    }
                };
                per.push((count, sum));
            }
            if exact {
                attrs.insert(name.to_owned(), (kind, per));
            }
        }
        let blocks = chunks.len();
        let mut used = 8 + attrs.keys().map(|n| attr_len(n, blocks)).sum::<usize>();
        while used > spare {
            let (name, _) = attrs.pop_last()?;
            used -= attr_len(&name, blocks);
        }
        (!attrs.is_empty()).then_some(Self { blocks, attrs })
    }

    /// Its bytes: the block count, the attribute count, then per attribute its name, kind and
    /// per block a `u32` count and an 8-byte sum.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend((self.blocks as u32).to_le_bytes());
        out.extend((self.attrs.len() as u32).to_le_bytes());
        for (name, (kind, per)) in &self.attrs {
            out.extend((name.len() as u32).to_le_bytes());
            out.extend(name.as_bytes());
            out.push(match kind {
                Kind::Int => 0,
                Kind::Float => 1,
            });
            for (count, sum) in per {
                out.extend(count.to_le_bytes());
                out.extend(sum.to_le_bytes());
            }
        }
        out
    }

    /// The section [`Self::encode`] wrote.
    ///
    /// # Errors
    /// `Corrupt` for a truncation, trailing bytes, or an unknown kind.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let corrupt = || FormatError::Corrupt("aggregate statistics that do not decode");
        let mut at = 0usize;
        let mut take = |n: usize| -> Result<&[u8], FormatError> {
            let end = at.checked_add(n).ok_or_else(corrupt)?;
            let s = bytes.get(at..end).ok_or_else(corrupt)?;
            at = end;
            Ok(s)
        };
        let u32_of = |s: &[u8]| s.try_into().map(u32::from_le_bytes).map_err(|_| corrupt());
        let blocks = u32_of(take(4)?)? as usize;
        let n = u32_of(take(4)?)? as usize;
        let mut attrs = BTreeMap::new();
        for _ in 0..n {
            let len = u32_of(take(4)?)? as usize;
            let name = std::str::from_utf8(take(len)?)
                .map_err(|_| corrupt())?
                .to_owned();
            let kind = match take(1)?.first() {
                Some(0) => Kind::Int,
                Some(1) => Kind::Float,
                _ => return Err(corrupt()),
            };
            let raw = take(blocks.checked_mul(12).ok_or_else(corrupt)?)?;
            let per = raw
                .chunks_exact(12)
                .map(|c| {
                    let (count, sum) = c.split_at(4);
                    Ok((
                        u32_of(count)?,
                        sum.try_into()
                            .map(u64::from_le_bytes)
                            .map_err(|_| corrupt())?,
                    ))
                })
                .collect::<Result<Vec<_>, FormatError>>()?;
            attrs.insert(name, (kind, per));
        }
        if at != bytes.len() {
            return Err(corrupt());
        }
        Ok(Self { blocks, attrs })
    }

    /// Block `block`'s statistics for `attr`, when it has them.
    #[must_use]
    pub fn get(&self, attr: &str, block: usize) -> Option<Stat> {
        let (kind, per) = self.attrs.get(attr)?;
        let &(count, sum) = per.get(block)?;
        Some(match kind {
            Kind::Int => Stat::Int {
                count,
                sum: sum as i64,
            },
            Kind::Float => Stat::Float {
                count,
                sum: f64::from_bits(sum),
            },
        })
    }

    /// How many blocks it covers.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.blocks
    }

    /// The attributes it covers, in order: what the benchmark prints.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.attrs.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    fn doc(attrs: &[(&str, Value)]) -> Document {
        let mut d = Document::new("x", vec![]);
        for (k, v) in attrs {
            d.attrs.insert((*k).to_owned(), v.clone());
        }
        d
    }

    #[test]
    fn single_kind_numbers_are_counted_and_summed_per_block() {
        let a = [
            doc(&[("i", Value::Int(3)), ("f", Value::Float(0.5))]),
            doc(&[("i", Value::Int(-1))]),
        ];
        let b = [doc(&[
            ("f", Value::Float(2.25)),
            ("s", Value::Str("x".into())),
        ])];
        let stats = Aggregates::build(&[&a, &b], 4096).unwrap();
        assert_eq!(stats.block_count(), 2);
        assert_eq!(stats.names(), ["f", "i"]);
        assert_eq!(stats.get("i", 0), Some(Stat::Int { count: 2, sum: 2 }));
        assert_eq!(stats.get("i", 1), Some(Stat::Int { count: 0, sum: 0 }));
        assert_eq!(stats.get("f", 0), Some(Stat::Float { count: 1, sum: 0.5 }));
        assert_eq!(stats.get("f", 1).map(Stat::count), Some(1));
        assert_eq!(stats.get("f", 2), None);
        assert_eq!(stats.get("s", 0), None);
        assert_eq!(Aggregates::decode(&stats.encode()).unwrap(), stats);
    }

    #[test]
    fn mixed_kinds_overflow_and_non_finite_sums_are_left_out() {
        let rows = [
            doc(&[
                ("mixed", Value::Int(1)),
                ("big", Value::Int(i64::MAX)),
                ("inf", Value::Float(f64::MAX)),
                ("u", Value::Uint(1)),
                ("ok", Value::Int(i64::MIN)),
            ]),
            doc(&[
                ("mixed", Value::Float(1.0)),
                ("big", Value::Int(1)),
                ("inf", Value::Float(f64::MAX)),
                ("ok", Value::Int(i64::MAX)),
            ]),
        ];
        let stats = Aggregates::build(&[&rows], 4096).unwrap();
        // `ok` sums to -1 through i128 although its running sum leaves i64.
        assert_eq!(stats.names(), ["ok"]);
        assert_eq!(stats.get("ok", 0), Some(Stat::Int { count: 2, sum: -1 }));
        let none = [doc(&[("s", Value::Str("a".into()))])];
        assert!(Aggregates::build(&[&none], 4096).is_none());
    }

    #[test]
    fn the_budget_keeps_the_first_names_that_fit() {
        let rows: Vec<Document> = (0..4)
            .map(|i| {
                doc(&[
                    ("a", Value::Int(i)),
                    ("bb", Value::Int(i)),
                    ("c", Value::Int(i)),
                ])
            })
            .collect();
        let chunks: Vec<&[Document]> = rows.chunks(1).collect();
        let all = Aggregates::build(&chunks, 4096).unwrap();
        let len = all.encode().len();
        assert_eq!(
            len,
            8 + attr_len("a", 4) + attr_len("bb", 4) + attr_len("c", 4)
        );
        assert!(Aggregates::build(&chunks, len).is_some_and(|s| s.names().len() == 3));
        let two = Aggregates::build(&chunks, len - 1).unwrap();
        assert_eq!(two.names(), ["a", "bb"]);
        assert!(two.encode().len() < len);
        assert!(Aggregates::build(&chunks, 8 + attr_len("a", 4) - 1).is_none());
    }

    #[test]
    fn a_truncated_or_malformed_section_is_refused() {
        let rows = [doc(&[("i", Value::Int(7)), ("f", Value::Float(1.5))])];
        let bytes = Aggregates::build(&[&rows], 4096).unwrap().encode();
        for cut in 0..bytes.len() {
            assert!(Aggregates::decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut long = bytes.clone();
        long.push(0);
        assert!(Aggregates::decode(&long).is_err());
        // The first attribute's kind byte, after the two counts, its name length and `f`.
        let mut kind = bytes.clone();
        kind[8 + 4 + 1] = 2;
        assert!(Aggregates::decode(&kind).is_err());
        let mut name = bytes;
        name[8 + 4] = 0xff;
        assert!(Aggregates::decode(&name).is_err());
    }
}
