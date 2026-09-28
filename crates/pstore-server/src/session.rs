//! The `session` token (M11.1): what a client carries so a read reflects its own durable
//! writes and never goes backwards.
//!
//! Opaque to clients and laid out here, base64url without padding:
//!
//! ```text
//! version (1) ‖ tenant (16) ‖ epoch (8) ‖ flags (1) ‖ n (1) ‖ n × (lane (8), next (8))
//! ```
//!
//! An entry `(lane, next)` says the session made a durable write to `lane` below `next`.
//! Flag bit 0 is **overflow**: the session wrote to more lanes than a token holds, so a read
//! must cover every acknowledged write -- `strong` -- rather than the ones it names.
//!
//! ⚠️ **Unsigned, deliberately, for now.** The protocol MACs the token so a forged far-future
//! one cannot make a node wait; this server never waits on a token, it refuses. A forged token
//! refuses its own reads or asks for a fold of its own tenant, which `strong` already can. The
//! MAC lands with the routing hint, where a forged token could steer load.

use base64::Engine as _;
use std::collections::BTreeMap;

/// The only layout this server writes or reads.
const VERSION: u8 = 1;
/// Lanes a token names before it overflows: 283 bytes at most.
pub(crate) const MAX_ENTRIES: usize = 16;
/// Everything before the entries.
const HEADER: usize = 1 + 16 + 8 + 1 + 1;
const OVERFLOW: u8 = 1;

/// A decoded session token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    /// The tenant it was minted for.
    pub(crate) tenant: u128,
    /// The newest epoch the session was served or committed.
    pub(crate) epoch: u64,
    /// Whether it outgrew [`MAX_ENTRIES`] and dropped its entries.
    pub(crate) overflow: bool,
    /// Lane to "every durable write below this sequence".
    pub(crate) entries: BTreeMap<u64, u64>,
}

impl Token {
    /// A session that has seen and written nothing.
    #[must_use]
    pub(crate) fn empty(tenant: u128) -> Self {
        Self {
            tenant,
            epoch: 0,
            overflow: false,
            entries: BTreeMap::new(),
        }
    }

    /// Reads a token presented for `tenant`, or says why it is not one.
    ///
    /// # Errors
    /// Not base64url, another version or length, another tenant, or more than
    /// [`MAX_ENTRIES`] entries.
    pub(crate) fn decode(text: &str, tenant: u128) -> Result<Self, String> {
        let b = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(text)
            .map_err(|e| format!("not base64url: {e}"))?;
        let (Some(head), Some(tail)) = (b.get(..HEADER), b.get(HEADER..)) else {
            return Err(format!("{} bytes is shorter than a token", b.len()));
        };
        let word = |at: usize, len: usize| -> u128 {
            head.get(at..at + len).map_or(0, |s| {
                s.iter()
                    .rev()
                    .fold(0u128, |acc, byte| (acc << 8) | u128::from(*byte))
            })
        };
        if head.first() != Some(&VERSION) {
            return Err(format!("version {:?} is not {VERSION}", head.first()));
        }
        if word(1, 16) != tenant {
            return Err("minted for another tenant".to_owned());
        }
        let n = usize::from(head.get(HEADER - 1).copied().unwrap_or(0));
        if n > MAX_ENTRIES {
            return Err(format!("{n} entries, over {MAX_ENTRIES}"));
        }
        if tail.len() != n * 16 {
            return Err(format!("{} entry bytes for {n} entries", tail.len()));
        }
        let u64_at = |s: &[u8]| -> u64 {
            s.iter()
                .rev()
                .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte))
        };
        let entries = tail
            .chunks_exact(16)
            .map(|e| e.split_at(8))
            .map(|(lane, next)| (u64_at(lane), u64_at(next)))
            .collect();
        Ok(Self {
            tenant,
            epoch: word(17, 8) as u64,
            overflow: head.get(HEADER - 2).is_some_and(|f| f & OVERFLOW != 0),
            entries,
        })
    }

    /// The header value.
    #[must_use]
    pub(crate) fn encode(&self) -> String {
        let mut b = Vec::with_capacity(HEADER + self.entries.len() * 16);
        b.push(VERSION);
        b.extend(self.tenant.to_le_bytes());
        b.extend(self.epoch.to_le_bytes());
        b.push(if self.overflow { OVERFLOW } else { 0 });
        b.push(self.entries.len() as u8);
        for (lane, next) in &self.entries {
            b.extend(lane.to_le_bytes());
            b.extend(next.to_le_bytes());
        }
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }

    /// Records a durable write to `lane` below `next`. A lane past [`MAX_ENTRIES`] overflows
    /// the token, which then names no lane at all: a read of it must be `strong`.
    pub(crate) fn wrote(&mut self, lane: u64, next: u64) {
        if self.overflow {
            return;
        }
        let at = self.entries.entry(lane).or_insert(0);
        *at = (*at).max(next);
        if self.entries.len() > MAX_ENTRIES {
            self.overflow = true;
            self.entries.clear();
        }
    }

    /// Raises the epoch; it never falls.
    pub(crate) fn saw(&mut self, epoch: u64) {
        self.epoch = self.epoch.max(epoch);
    }

    /// Merges another token of the same session: the max of each.
    pub(crate) fn merge(&mut self, other: &Self) {
        self.saw(other.epoch);
        if other.overflow {
            self.overflow = true;
            self.entries.clear();
        }
        for (lane, next) in &other.entries {
            self.wrote(*lane, *next);
        }
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

    #[test]
    fn a_token_round_trips_at_every_size_it_can_hold() {
        for n in 0..=MAX_ENTRIES as u64 {
            let mut t = Token::empty(u128::MAX - 3);
            t.saw(u64::MAX - 1);
            for lane in 0..n {
                t.wrote(lane * 0x0101_0101_0101, u64::MAX - lane);
            }
            let text = t.encode();
            assert_eq!(Token::decode(&text, u128::MAX - 3).unwrap(), t, "{n}");
        }
    }

    #[test]
    fn the_seventeenth_lane_overflows_and_overflow_is_sticky() {
        let mut t = Token::empty(1);
        for lane in 0..MAX_ENTRIES as u64 {
            t.wrote(lane, 1);
        }
        assert!(!t.overflow);
        t.wrote(3, 9);
        assert_eq!(
            t.entries.len(),
            MAX_ENTRIES,
            "an existing lane is not a new one"
        );
        assert!(!t.overflow);
        t.wrote(99, 1);
        assert!(t.overflow);
        assert!(t.entries.is_empty());
        t.wrote(5, 5);
        assert!(t.entries.is_empty(), "an overflowed token names no lane");
        let back = Token::decode(&t.encode(), 1).unwrap();
        assert!(back.overflow);
    }

    #[test]
    fn a_merge_takes_the_max_of_everything() {
        let mut a = Token::empty(1);
        a.saw(5);
        a.wrote(1, 3);
        a.wrote(2, 9);
        let mut b = Token::empty(1);
        b.saw(4);
        b.wrote(1, 7);
        b.wrote(3, 1);
        a.merge(&b);
        assert_eq!(a.epoch, 5);
        assert_eq!(
            a.entries,
            BTreeMap::from([(1, 7), (2, 9), (3, 1)]),
            "per lane, the max"
        );
        let mut o = Token::empty(1);
        o.overflow = true;
        a.merge(&o);
        assert!(a.overflow && a.entries.is_empty());
    }
}
