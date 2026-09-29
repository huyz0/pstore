//! The trigram sketch (M15.2): a Bloom filter per block per declared attribute, over the
//! trigrams of its string values, so a pattern filter can skip a block the way a zone map does.
//!
//! ⚠️ **Folded by simple case folding**, one character to one: [`fold`] maps each character to
//! the smallest code point of its simple-case-folding class -- the relation `(?i)` uses. So one
//! fold serves case-sensitive and case-insensitive patterns alike, a literal's trigrams are
//! always among a matching value's, and one edit touches at most three trigrams. Lowercasing
//! is none of those things (spec review, M15: final sigma, `ſ`, `İ`).
//!
//! ⚠️ **Sound, not exact.** A filter may say "maybe" wrongly and never "no" wrongly: a block is
//! skipped only when a trigram every match must contain is absent from its filter.

use crate::FormatError;
use std::collections::BTreeMap;

/// Three folded characters.
pub type Trigram = [char; 3];

/// The smallest code point in `c`'s simple-case-folding class.
#[must_use]
pub fn fold(c: char) -> char {
    if c.is_ascii() {
        // Every ASCII letter's class has its uppercase as its least member (`K` and `S` share
        // theirs with U+212A and `ſ`, which are larger).
        return c.to_ascii_uppercase();
    }
    let mut class =
        regex_syntax::hir::ClassUnicode::new([regex_syntax::hir::ClassUnicodeRange::new(c, c)]);
    class.case_fold_simple();
    class.ranges().first().map_or(c, |r| r.start().min(c))
}

/// The distinct folded trigrams of `s`, in order of first appearance.
#[must_use]
pub fn trigrams(s: &str) -> Vec<Trigram> {
    let folded: Vec<char> = s.chars().map(fold).collect();
    let mut out: Vec<Trigram> = Vec::new();
    for w in folded.windows(3) {
        if let [a, b, c] = *w
            && !out.contains(&[a, b, c])
        {
            out.push([a, b, c]);
        }
    }
    out
}

/// The two positions a trigram sets in a filter of `bits`: FNV-1a 64 over its UTF-8, seeded.
fn positions(t: &Trigram, bits: u32) -> [u32; 2] {
    let utf8: String = t.iter().collect();
    [0u64, 0x9e37_79b9].map(|seed| {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
        for b in utf8.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "reduced modulo a u32 first"
        )]
        let at = (h % u64::from(bits)) as u32;
        at
    })
}

/// The smallest and largest bits a block's filter may have.
pub const MIN_BITS: u32 = 64;
/// See [`MIN_BITS`].
pub const MAX_BITS: u32 = 2048;

/// Every declared attribute's filter for every block of one segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sketch {
    bits: u32,
    blocks: usize,
    /// Per attribute, `blocks` filters of `bits / 8` bytes, one after another.
    filters: BTreeMap<String, Vec<u8>>,
}

impl Sketch {
    /// Builds the filters of `attrs` over `blocks` blocks, from each block's string values.
    ///
    /// # Panics
    /// Never: `bits` is clamped to a power of two in `[MIN_BITS, MAX_BITS]`.
    #[must_use]
    pub fn build(
        attrs: &[String],
        blocks: usize,
        bits: u32,
        values: impl Fn(&str, usize) -> Vec<String>,
    ) -> Self {
        let bits = bits
            .clamp(MIN_BITS, MAX_BITS)
            .next_power_of_two()
            .min(MAX_BITS);
        let width = (bits / 8) as usize;
        let filters = attrs
            .iter()
            .map(|a| {
                let mut f = vec![0u8; width * blocks];
                for b in 0..blocks {
                    for v in values(a, b) {
                        for t in trigrams(&v) {
                            for p in positions(&t, bits) {
                                if let Some(byte) = f.get_mut(b * width + (p / 8) as usize) {
                                    *byte |= 1 << (p % 8);
                                }
                            }
                        }
                    }
                }
                (a.clone(), f)
            })
            .collect();
        Self {
            bits,
            blocks,
            filters,
        }
    }

    /// Its encoded length for `attrs` at `bits` over `blocks`, without building it.
    #[must_use]
    pub fn encoded_len(attrs: &[String], blocks: usize, bits: u32) -> usize {
        let names: usize = attrs.iter().map(|a| 4 + a.len()).sum();
        4 + names + 4 + 4 + attrs.len() * blocks * (bits / 8) as usize
    }

    /// The widest filters -- a power of two from [`MAX_BITS`] down to [`MIN_BITS`] -- whose
    /// sketch of `attrs` over `blocks` fits in `spare` bytes, or `None` when even the narrowest
    /// does not.
    #[must_use]
    pub fn widest(attrs: &[String], blocks: usize, spare: usize) -> Option<u32> {
        let mut bits = MAX_BITS;
        while bits >= MIN_BITS && Self::encoded_len(attrs, blocks, bits) > spare {
            bits /= 2;
        }
        (bits >= MIN_BITS).then_some(bits)
    }

    /// Its bytes: the names, `bits`, the block count, then each attribute's filters.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend((self.filters.len() as u32).to_le_bytes());
        for name in self.filters.keys() {
            out.extend((name.len() as u32).to_le_bytes());
            out.extend(name.as_bytes());
        }
        out.extend(self.bits.to_le_bytes());
        out.extend((self.blocks as u32).to_le_bytes());
        for f in self.filters.values() {
            out.extend(f);
        }
        out
    }

    /// The sketch [`Self::encode`] wrote.
    ///
    /// # Errors
    /// Anything else: a truncation, trailing bytes, or a width that is not a power of two in
    /// range.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut c = Cursor { bytes, at: 0 };
        let n = c.u32()? as usize;
        let mut names = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            let len = c.u32()? as usize;
            let name = std::str::from_utf8(c.take(len)?).map_err(|_| corrupt())?;
            names.push(name.to_owned());
        }
        let bits = c.u32()?;
        if !(MIN_BITS..=MAX_BITS).contains(&bits) || !bits.is_power_of_two() {
            return Err(corrupt());
        }
        let blocks = c.u32()? as usize;
        let width = (bits / 8) as usize;
        let mut filters = BTreeMap::new();
        for name in names {
            let f = c
                .take(width.checked_mul(blocks).ok_or_else(corrupt)?)?
                .to_vec();
            filters.insert(name, f);
        }
        if c.at != bytes.len() {
            return Err(corrupt());
        }
        Ok(Self {
            bits,
            blocks,
            filters,
        })
    }

    /// Whether block `block`'s filter for `attr` could hold every one of `required` -- `true`
    /// when the attribute has no filter here, which rules nothing out.
    #[must_use]
    pub fn may_hold(&self, attr: &str, block: usize, required: &[Trigram]) -> bool {
        self.held(attr, block, required)
            .is_none_or(|n| n == required.len())
    }

    /// How many of `trigrams` block `block`'s filter for `attr` could hold, or `None` when the
    /// attribute has no filter here.
    #[must_use]
    pub fn held(&self, attr: &str, block: usize, trigrams: &[Trigram]) -> Option<usize> {
        let f = self.filters.get(attr)?;
        let width = (self.bits / 8) as usize;
        let slice = f.get(block * width..(block + 1) * width)?;
        Some(
            trigrams
                .iter()
                .filter(|t| {
                    positions(t, self.bits).iter().all(|p| {
                        slice
                            .get((p / 8) as usize)
                            .is_some_and(|byte| byte & (1 << (p % 8)) != 0)
                    })
                })
                .count(),
        )
    }

    /// How many blocks it covers.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.blocks
    }
}

fn corrupt() -> FormatError {
    FormatError::Corrupt("a trigram sketch that does not decode")
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
