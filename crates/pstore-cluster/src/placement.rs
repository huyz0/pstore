//! Local Rendezvous Hashing with overload skipping (D-5).
//!
//! ## Why not plain rendezvous, and why not consistent hashing
//!
//! Plain rendezvous hashing ranks *every* node per key: optimal churn, `O(N)` per placement,
//! which at 10,000 nodes is 10,000 hashes on a path that runs per query. Consistent hashing
//! is `O(log N)` but its load imbalance is 50%+ without heavy virtual-node counts.
//!
//! **Local** rendezvous takes the `C` nodes clockwise of the key's ring position and ranks
//! only those: `O(log N)` to find the window, `O(C)` to rank it. Measured, that costs
//! **1.2–1.6× the optimal churn** — the price of looking at a window instead of the fleet,
//! and a number the corpus asserts qualitatively but does not give.

use crate::Roster;
use std::collections::BTreeSet;

/// The smallest window, and D-5's stated C ≈ 32.
pub const MIN_WINDOW: usize = 32;

/// Nodes considered per key, for a fleet of `n`.
///
/// ⚠️ **C must grow with N.** D-5 states "C ≈ 32" as a constant, and measured that is only
/// adequate for a small fleet: with C fixed at 32 the hottest node holds 1.208× the mean at
/// N=100 and **1.811× at N=2000**, because a fixed window covers an ever-smaller slice of an
/// ever-more-variable ring. Growing it as `3·√N` holds the figure near 1.48 at N=2000, at
/// `O(√N)` work per placement instead of `O(1)` — still far below plain rendezvous's `O(N)`.
///
/// ⚠️ Neither reproduces `routing-and-placement.md`'s "max load within 10–15% of average".
/// Recorded as a correction rather than absorbed into a tolerance.
#[must_use]
pub fn window(n: usize) -> usize {
    // `isqrt` rather than a float: placement must agree byte for byte across machines, and
    // floating point is the wrong tool for a decision every node has to reach identically.
    MIN_WINDOW.max(3 * n.isqrt()).min(n)
}

/// A placement function over one roster.
#[derive(Debug, Clone)]
pub struct Placement<'a> {
    roster: &'a Roster,
    overloaded: BTreeSet<&'a str>,
}

impl<'a> Placement<'a> {
    /// Placement over `roster`, with nothing marked hot.
    #[must_use]
    pub fn new(roster: &'a Roster) -> Self {
        Self {
            roster,
            overloaded: BTreeSet::new(),
        }
    }

    /// Marks nodes as overloaded, so placement steps past them (CHBL-style).
    ///
    /// ⚠️ Real load beats hashed load whenever they disagree: the hash cannot know about a
    /// single 50 TB index or one at 100× its neighbours' query rate.
    #[must_use]
    pub fn with_overloaded<I: IntoIterator<Item = &'a str>>(mut self, hot: I) -> Self {
        self.overloaded = hot.into_iter().collect();
        self
    }

    /// The `r` nodes that should serve `key`, best first.
    ///
    /// **Zero blob requests**: a pure function of the roster, which is why a fleet change
    /// costs nothing but cache locality.
    #[must_use]
    pub fn place(&self, key: &str, r: usize) -> Vec<&'a str> {
        let ring = self.roster.ring();
        // ⚠️ No early return for an empty ring or `r == 0` (M8h): both already fall out as an
        // empty list -- `window(0)` is 0, so an empty ring iterates nothing, and `take(0)` plus the
        // relax loop's own guard return nothing for `r == 0`. Once the ring search lost its
        // redundant `%`, this guard became redundant too, and its `||` mutant equivalent.
        let pos = hash(&[key.as_bytes()]);
        // First node clockwise of the key. The ring is sorted, so this is a binary search.
        let start = ring.partition_point(|(p, _)| *p < pos);
        let window = window(ring.len());

        // ⚠️ The wrap is HERE, and only here: `% ring.len()` is what makes it a ring rather than a
        // line, and `start` may be `ring.len()`. A second `%` on `start` itself was redundant --
        // its `+` mutant was equivalent, and M8h deleted it rather than excluding it.
        let mut ranked: Vec<(u64, &'a str)> = (0..window)
            .filter_map(|i| ring.get((start + i) % ring.len()))
            .map(|(_, id)| (hash(&[key.as_bytes(), id.as_bytes()]), id.as_str()))
            .collect();
        // Descending weight; ties by id so two nodes with the same roster always agree.
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));

        let mut out: Vec<&'a str> = ranked
            .iter()
            .filter(|(_, id)| !self.overloaded.contains(id))
            .map(|(_, id)| *id)
            .take(r)
            .collect();

        // ⚠️ **Never fail.** If skipping the hot nodes leaves fewer than `r`, relax and take
        // them anyway: a short placement list silently under-replicates, and nothing
        // downstream reports it. An overloaded node serving a request is a slow answer; no
        // node serving it is no answer.
        //
        // ⚠️ No outer `if out.len() < r.min(window)`: the loop's own guard makes it redundant --
        // its `<=` mutant was equivalent, and M8h deleted it rather than excluding it.
        for (_, id) in &ranked {
            if out.len() >= r.min(window) {
                break;
            }
            if !out.contains(id) {
                out.push(id);
            }
        }
        out
    }
}

/// A node's position on the ring.
#[must_use]
pub(crate) fn ring_position(node: &str) -> u64 {
    hash(&[b"ring", node.as_bytes()])
}

/// FNV-1a over the parts, separated so `("ab","c")` and `("a","bc")` differ.
///
/// Deliberately not a cryptographic hash and deliberately written out: placement must agree
/// **byte for byte across processes and versions**, so it cannot depend on a crate's default
/// hasher — `RandomState` is seeded per process, which would make every node place
/// differently and no test in one process could see it.
/// The FNV-1a 64-bit prime, `1099511628211`.
///
/// ⚠️ Named rather than inlined because it was **wrong** as a literal: `0x1000_0000_01b3`
/// groups to `0x1000000001b3`, sixteen times the real prime, and an extra zero hides
/// perfectly inside underscore grouping. It still hashed — any odd multiplier does — just
/// with worse avalanche, which showed up only as placement imbalance of 1.347x against the
/// 1.205x the same algorithm achieves with the right constant. No test of the hash itself
/// would have caught it.
const FNV_PRIME: u64 = 0x100_0000_01b3;

fn hash(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            h ^= 0xff;
            h = h.wrapping_mul(FNV_PRIME);
        }
        for b in *p {
            h ^= u64::from(*b);
            h = h.wrapping_mul(FNV_PRIME);
        }
    }
    // ⚠️ The **full** murmur3 finalizer, not half of it. FNV-1a alone spreads short strings
    // poorly, and an incomplete mix showed up as measurable imbalance rather than as a bug:
    // max/mean 1.312 against 1.205 for a strong hash, and churn 1.73x optimal against 1.57x.
    // The numbers this milestone pins were measured with blake2b in simulation, so a weaker
    // hash here would have meant the criteria described a different function than the code —
    // caught because the thresholds were pinned from measurement *before* implementation.
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ⚠️ Values from an independent model of FNV-1a with a `0xff` part separator and the full
    /// murmur3 finaliser, compared on ALL 64 bits. The last finaliser step changes only the low
    /// 31 bits, and every comparison `place` makes is decided by the high bits -- so its `|=`
    /// mutant passed the golden placement test, and M8h's sweep found it surviving.
    #[test]
    fn the_hash_matches_an_independent_model() {
        assert_eq!(hash(&[b"ring", b"n1"]), 0x8eba_09a8_eac0_750a);
        assert_eq!(hash(&[b"idx0/s0", b"10.0.0.1:7946"]), 0x057c_0ebc_93b3_0547);
        assert_eq!(hash(&[b"a"]), 0x82a2_a958_a9be_ce5b);
    }

    #[test]
    fn parts_are_separated() {
        // Without a separator, `("ab", "c")` and `("a", "bc")` collide — so two different
        // keys would place identically and a hot key could not be told from its neighbour.
        assert_ne!(hash(&[b"ab", b"c"]), hash(&[b"a", b"bc"]));
    }
}
