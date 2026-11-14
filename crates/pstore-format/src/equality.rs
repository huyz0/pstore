//! The equality section (M230): per block, what a string or bool equality can rule out.
//!
//! Zone maps prune numbers and datetimes; a string or a bool has no useful `(min, max)`, so
//! before this `category = "shoes"` read every block. Here, per block:
//!
//! - a **bool** attribute has two bits: some row holds `true`, some row holds `false`;
//! - a **string** attribute has a Bloom filter over its string values (three probes from one
//!   64-bit hash), every filter one width, chosen by the writer from the budget it has.
//!
//! ⚠️ **Sound, not exact.** A block is ruled out only when its filter proves no row holds the
//! literal. A block where some row of a string attribute holds a datetime, uuid or bytes is
//! **open** for it: a string literal can equal such a row (M9h.3, M171) by a spelling that was
//! never hashed, so an open block is never ruled out. Numbers and arrays never equal a string
//! or bool literal (`filter.rs` `compare`), so they leave a block closed.
//!
//! ⚠️ The hash is part of the format: a section written by one build is read by the next.

use crate::{FormatError, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The narrowest and widest a string filter may be, in bits.
pub const MIN_BITS: u32 = 64;
/// See [`MIN_BITS`].
pub const MAX_BITS: u32 = 2048;
/// Probes a value sets in its block's filter.
const PROBES: u32 = 3;

/// The equality filters of one segment's blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Equality {
    bits: u32,
    blocks: usize,
    /// Per string attribute: an open bitmap of `ceil(blocks / 8)` bytes, then `blocks`
    /// filters of `bits / 8` bytes.
    strings: BTreeMap<String, Vec<u8>>,
    /// Per bool attribute: two bits a block, `true` then `false`, in `ceil(blocks / 4)` bytes.
    bools: BTreeMap<String, Vec<u8>>,
}

/// A value's hash: FNV-1a 64 over its bytes, then SplitMix64's finaliser, so the low bits a
/// power-of-two width keeps depend on every byte.
#[must_use]
pub fn hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// The bits a hash sets in a filter of `bits`: double hashing, `h1 + i * h2`.
fn probes(h: u64, bits: u32) -> impl Iterator<Item = u32> {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the two halves of the hash"
    )]
    let (h1, h2) = (h as u32, ((h >> 32) as u32) | 1);
    (0..PROBES).map(move |i| h1.wrapping_add(i.wrapping_mul(h2)) % bits)
}

fn bit(bytes: &[u8], at: usize) -> bool {
    bytes.get(at / 8).is_some_and(|b| b & (1 << (at % 8)) != 0)
}

fn set(bytes: &mut [u8], at: usize) {
    if let Some(b) = bytes.get_mut(at / 8) {
        *b |= 1 << (at % 8);
    }
}

/// Whether a string attribute's filters could rule blocks out for values the segment holds:
/// its blocks hold, on average, at most half of its distinct values (a block without the
/// attribute holds none). A `category` of 40
/// values each in every block is not, and its filters would prune only a value no row has.
fn selective(seen: &[Seen]) -> bool {
    let all: BTreeSet<u64> = seen.iter().flat_map(|s| s.hashes.iter().copied()).collect();
    let total: usize = seen.iter().map(|s| s.hashes.len()).sum();
    // mean(n_b) <= |all| / 2 over every block, without dividing.
    total * 2 <= all.len() * seen.len()
}

/// What one block holds of one string attribute: its distinct hashes, or open.
#[derive(Default)]
struct Seen {
    hashes: BTreeSet<u64>,
    open: bool,
}

impl Equality {
    /// The section for `blocks` blocks whose `(attribute, value)` pairs `values` yields, block
    /// by block, at the widest filters that fit `spare` bytes, or `None` when nothing does.
    ///
    /// A string attribute is left out when its blocks hold on average more than half its
    /// distinct values (it could prune only values no row holds), and at a width a quarter of
    /// which its busiest block's values would fill. Widths are tried widest first, and at each
    /// the attributes with the fewest values are kept while the rest are dropped to fit; with
    /// no string attribute left at any width, the bools alone are written if they fit.
    #[must_use]
    pub fn build<'a, I: Iterator<Item = (&'a str, &'a Value)>>(
        blocks: usize,
        spare: usize,
        values: impl Fn(usize) -> I,
    ) -> Option<Self> {
        let mut strings: BTreeMap<String, Vec<Seen>> = BTreeMap::new();
        let mut bools: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for b in 0..blocks {
            for (name, v) in values(b) {
                match v {
                    Value::Str(s) => {
                        strings
                            .entry(name.to_owned())
                            .or_insert_with(|| (0..blocks).map(|_| Seen::default()).collect())
                            .get_mut(b)?
                            .hashes
                            .insert(hash(s.as_bytes()));
                    }
                    Value::DateTime(_) | Value::Uuid(_) | Value::Bytes(_) => {
                        strings
                            .entry(name.to_owned())
                            .or_insert_with(|| (0..blocks).map(|_| Seen::default()).collect())
                            .get_mut(b)?
                            .open = true;
                    }
                    Value::Bool(t) => {
                        let row = bools
                            .entry(name.to_owned())
                            .or_insert_with(|| vec![0; blocks.div_ceil(4)]);
                        set(row, 2 * b + usize::from(!*t));
                    }
                    Value::Int(_)
                    | Value::Float(_)
                    | Value::Uint(_)
                    | Value::Array(_)
                    | Value::Null => {}
                }
            }
        }
        // An attribute only ever open, or holding no string, needs no filter: absent rules
        // nothing out, which is what all-open would say.
        strings.retain(|_, seen| seen.iter().any(|s| !s.hashes.is_empty()));
        // Per attribute, its busiest block's distinct strings, if it can prune at all.
        let mut busiest: Vec<(&String, usize)> = strings
            .iter()
            .filter(|(_, seen)| selective(seen))
            .map(|(n, seen)| (n, seen.iter().map(|s| s.hashes.len()).max().unwrap_or(0)))
            .collect();
        busiest.sort_by_key(|(n, most)| (*most, *n));
        let bools_len: usize = bools.iter().map(|(n, f)| 4 + n.len() + f.len()).sum();
        let string_len = |bits: u32, names: &[(&String, usize)]| -> usize {
            names
                .iter()
                .map(|(n, _)| 4 + n.len() + blocks.div_ceil(8) + blocks * (bits / 8) as usize)
                .sum()
        };
        let mut bits = MAX_BITS;
        while bits >= MIN_BITS {
            // ⚠️ At most a quarter of the bits set by the busiest block's values, so its false
            // positive rate stays near 10% (three probes); beyond that the filter is mostly
            // ones and costs budget a narrower, selective attribute could use. Fewest values
            // first, so an attribute is dropped at this width before the width is halved.
            let mut chosen: Vec<(&String, usize)> = busiest
                .iter()
                .copied()
                .filter(|(_, most)| most * 4 <= bits as usize)
                .collect();
            while !chosen.is_empty() && 16 + bools_len + string_len(bits, &chosen) > spare {
                chosen.pop();
            }
            if !chosen.is_empty() {
                let mut out = Self {
                    bits,
                    blocks,
                    strings: BTreeMap::new(),
                    bools: bools.clone(),
                };
                for (name, _) in chosen {
                    out.strings
                        .insert(name.clone(), Self::filters(strings.get(name)?, bits));
                }
                return Some(out);
            }
            bits /= 2;
        }
        let out = Self {
            bits: MIN_BITS,
            blocks,
            strings: BTreeMap::new(),
            bools,
        };
        (!out.bools.is_empty() && out.encoded_len() <= spare).then_some(out)
    }

    /// One string attribute's open bitmap and filters, at `bits`.
    fn filters(seen: &[Seen], bits: u32) -> Vec<u8> {
        let width = (bits / 8) as usize;
        let head = seen.len().div_ceil(8);
        let mut out = vec![0u8; head + width * seen.len()];
        for (b, s) in seen.iter().enumerate() {
            if s.open {
                set(&mut out, b);
            }
            if let Some(f) = out.get_mut(head + b * width..head + (b + 1) * width) {
                for h in &s.hashes {
                    for p in probes(*h, bits) {
                        set(f, p as usize);
                    }
                }
            }
        }
        out
    }

    /// Its encoded length.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        let names = |m: &BTreeMap<String, Vec<u8>>| -> usize {
            m.iter().map(|(n, f)| 4 + n.len() + f.len()).sum()
        };
        4 + 4 + 4 + 4 + names(&self.strings) + names(&self.bools)
    }

    /// Its bytes: `bits`, the block count, the string names, the bool names, then each string
    /// attribute's bitmap and filters and each bool attribute's bits, in name order.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend(self.bits.to_le_bytes());
        out.extend((self.blocks as u32).to_le_bytes());
        for m in [&self.strings, &self.bools] {
            out.extend((m.len() as u32).to_le_bytes());
            for name in m.keys() {
                out.extend((name.len() as u32).to_le_bytes());
                out.extend(name.as_bytes());
            }
        }
        for m in [&self.strings, &self.bools] {
            for f in m.values() {
                out.extend(f);
            }
        }
        out
    }

    /// The section [`Self::encode`] wrote.
    ///
    /// # Errors
    /// `Corrupt` for a truncation, trailing bytes, or a width that is not a power of two in
    /// range.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut c = Cursor { bytes, at: 0 };
        let bits = c.u32()?;
        if !(MIN_BITS..=MAX_BITS).contains(&bits) || !bits.is_power_of_two() {
            return Err(corrupt());
        }
        let blocks = c.u32()? as usize;
        let mut names: [Vec<String>; 2] = [Vec::new(), Vec::new()];
        for list in &mut names {
            let n = c.u32()? as usize;
            for _ in 0..n {
                let len = c.u32()? as usize;
                let name = std::str::from_utf8(c.take(len)?).map_err(|_| corrupt())?;
                list.push(name.to_owned());
            }
        }
        let [string_names, bool_names] = names;
        let string_len = blocks
            .checked_mul((bits / 8) as usize)
            .and_then(|n| n.checked_add(blocks.div_ceil(8)))
            .ok_or_else(corrupt)?;
        let mut strings = BTreeMap::new();
        for name in string_names {
            strings.insert(name, c.take(string_len)?.to_vec());
        }
        let mut bools = BTreeMap::new();
        for name in bool_names {
            bools.insert(name, c.take(blocks.div_ceil(4))?.to_vec());
        }
        if c.at != bytes.len() {
            return Err(corrupt());
        }
        Ok(Self {
            bits,
            blocks,
            strings,
            bools,
        })
    }

    /// Whether some row of block `block` could hold `attr` equal to `literal`: `false` only
    /// when this section proves none does. An attribute it does not name, a literal other than
    /// a string or a bool, and a block it does not cover rule nothing out.
    #[must_use]
    pub fn may_equal(&self, attr: &str, block: usize, literal: &Value) -> bool {
        if block >= self.blocks {
            return true;
        }
        match literal {
            Value::Str(s) => self.strings.get(attr).is_none_or(|f| {
                let width = (self.bits / 8) as usize;
                let head = self.blocks.div_ceil(8);
                let filter = f
                    .get(head + block * width..head + (block + 1) * width)
                    .unwrap_or_default();
                bit(f, block)
                    || probes(hash(s.as_bytes()), self.bits).all(|p| bit(filter, p as usize))
            }),
            Value::Bool(t) => self
                .bools
                .get(attr)
                .is_none_or(|f| bit(f, 2 * block + usize::from(!t))),
            _ => true,
        }
    }

    /// How many blocks it covers.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.blocks
    }

    /// Its filters' width in bits, and the string and bool attributes it names: what the
    /// benchmark prints.
    #[must_use]
    pub fn shape(&self) -> (u32, Vec<&str>, Vec<&str>) {
        (
            self.bits,
            self.strings.keys().map(String::as_str).collect(),
            self.bools.keys().map(String::as_str).collect(),
        )
    }
}

fn corrupt() -> FormatError {
    FormatError::Corrupt("an equality section that does not decode")
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        let end = self.at.checked_add(n).ok_or_else(corrupt)?;
        let s = self.bytes.get(self.at..end).ok_or_else(corrupt)?;
        self.at = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, FormatError> {
        let s = self.take(4)?;
        s.try_into().map(u32::from_le_bytes).map_err(|_| corrupt())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    reason = "assertions in tests are the reporting mechanism"
)]
mod tests {
    use super::*;

    /// A filter's false positive rate is near its design: 3 probes at 8 bits a value is about
    /// 3.1%. Probes that collapse to neighbouring bits (an `h2` of 1) or lose the odd step
    /// that keeps them apart cost several times that, and soundness alone would not notice.
    #[test]
    fn a_filter_s_false_positive_rate_is_near_its_design() {
        for bits in [512u32, 2048] {
            let n = bits / 8;
            let mut f = vec![0u8; bits as usize / 8];
            for i in 0..n {
                for at in probes(hash(format!("in-{i}").as_bytes()), bits) {
                    set(&mut f, at as usize);
                }
            }
            for i in 0..n {
                assert!(
                    probes(hash(format!("in-{i}").as_bytes()), bits).all(|at| bit(&f, at as usize))
                );
            }
            let asked = 50_000;
            let fp = (0..asked)
                .filter(|i| {
                    probes(hash(format!("out-{i}").as_bytes()), bits).all(|at| bit(&f, at as usize))
                })
                .count();
            assert!(
                fp * 100 < asked * 5,
                "{bits} bits: {fp} of {asked} false positives"
            );
        }
    }

    /// The probes of one value are distinct bits, so each adds evidence.
    #[test]
    fn a_value_s_probes_are_distinct_bits() {
        for i in 0..2_000 {
            let mut p: Vec<u32> = probes(hash(format!("v{i}").as_bytes()), 2048).collect();
            p.sort_unstable();
            p.dedup();
            assert_eq!(p.len(), PROBES as usize, "value {i}");
        }
    }

    /// A width out of range, or in range and not a power of two, is refused.
    #[test]
    fn a_width_must_be_a_power_of_two_in_range() {
        let ok = Equality {
            bits: 64,
            blocks: 1,
            strings: BTreeMap::new(),
            bools: BTreeMap::from([("b".to_owned(), vec![0])]),
        };
        let bytes = ok.encode();
        assert_eq!(Equality::decode(&bytes).unwrap(), ok);
        for bits in [96u32, 32, 4096, 0, MAX_BITS - 1] {
            let mut b = bytes.clone();
            b[..4].copy_from_slice(&bits.to_le_bytes());
            assert!(Equality::decode(&b).is_err(), "{bits} bits");
        }
    }
}
