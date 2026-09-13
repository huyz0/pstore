//! Shared identifier and version types for `pstore`.
//!
//! Layer 0 of the workspace: **types only, no logic, no dependencies**. That rule
//! is what stops this crate becoming a junk drawer (see
//! `docs/research/09-rust-stack/engineering-standards.md`, D-108 and OQ-160).
//!
//! Every identifier is a distinct newtype rather than a bare integer. Blob keys are
//! derived by mixing five of these values together, so passing them in the wrong
//! order is the most likely silent bug in the system — these types make it a
//! compile error instead.

/// A tenant: the unit of physical grouping, commit, and compare-and-swap.
///
/// Distinct from [`IndexId`] on purpose. A tenant owns many indexes and they commit
/// together under one CAS register, which is worth ~50x on commit cost at our
/// tenancy shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TenantId(pub u128);

/// An index: the unit of API, schema, query, and isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexId(pub u128);

/// A horizontal partition of one index, assigned by hash of the document id.
///
/// A document's shard never changes, because the hash of its id never changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShardId(pub u16);

/// A monotonic version of a tenant's committed state. Advancing it is a snapshot.
///
/// Ordering is the whole point: a reader holding epoch *E* has a complete,
/// self-consistent immutable view, and a stale writer's CAS fails because it names
/// an epoch that is no longer current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Epoch(pub u64);

impl Epoch {
    /// The epoch of a tenant that has never committed.
    pub const ZERO: Self = Self(0);

    /// The next epoch. Saturating, because an epoch must never wrap: wrapping would
    /// let a stale writer's CAS succeed against a future state.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// A single-writer append-only WAL stream, owned by one node.
///
/// Lanes are what keep bulk writes off the CAS path: N writers use N lanes and never
/// contend. Ordering is established at read time by merging lanes, not at write time
/// by serialising writers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LaneId(pub u64);

/// Position within a single [`LaneId`]. Dense and monotonic, so a reader can find the
/// tail by probing forward rather than listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Seq(pub u64);

impl Seq {
    /// The first position in a lane.
    pub const ZERO: Self = Self(0);

    /// The next position. Saturating, for the same reason as [`Epoch::next`].
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// An immutable segment. Globally unique and never reused, which is why cache entries
/// keyed by it never need invalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SegmentId(pub u128);

/// LSM level of a segment. Bounds how many segments a query must open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Level(pub u8);

/// An opaque compare-and-swap token: an S3/Azure ETag or a GCS generation.
///
/// **Never parsed, never compared for anything but equality.** ETags are not uniformly
/// content hashes — a multipart upload yields `md5(concat(part_md5s))-N`, and SSE-KMS
/// yields something that is not MD5 at all. Content integrity uses our own hash,
/// stored separately.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CasTag(String);

impl CasTag {
    /// Wraps a backend-supplied token.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The raw token, for sending back to the backend in a precondition header.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a compare-and-swap did not succeed.
///
/// `Lost` and `Contended` are distinct types rather than status codes because
/// conflating S3's 412 and 409 causes needless rebase storms: 412 means another
/// writer won and we must rebase, while 409 means the backend could not evaluate the
/// condition and we should simply retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasFailure {
    /// Precondition failed (HTTP 412): another writer won. Rebase, then retry.
    Lost,
    /// Condition could not be evaluated (HTTP 409): a concurrent write was in flight.
    /// Retry **without** rebasing — we did not lose.
    Contended,
}

/// How many times a CAS is rebased before giving up.
///
/// Bounded rather than `loop`: the exit condition is "no one else won this round", and a
/// backend that is permanently contended would otherwise spin forever on a path a caller
/// waits on.
///
/// ⚠️ **Here rather than in the crate that enforces it**, because a second crate now has to
/// agree with it. `pstore-testkit`'s sensitivity sweep models the caller that gives up, and a
/// matching `16` with a comment beside it would drift the first time this number moved — the
/// sweep would then report a threshold for a budget nobody has. M0c.
pub const MAX_CAS_ATTEMPTS: u32 = 16;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "assertions in tests may panic")]
mod tests {
    use super::*;

    #[test]
    fn epoch_orders_and_advances() {
        assert_eq!(Epoch::ZERO.next(), Epoch(1));
        assert!(Epoch(1) < Epoch(2));
        assert_eq!(Epoch(5).next().next(), Epoch(7));
    }

    #[test]
    fn epoch_saturates_rather_than_wrapping() {
        // Wrapping would let a stale writer's CAS succeed against a future state.
        assert_eq!(Epoch(u64::MAX).next(), Epoch(u64::MAX));
    }

    #[test]
    fn seq_orders_and_saturates() {
        assert_eq!(Seq::ZERO.next(), Seq(1));
        assert!(Seq(1) < Seq(2));
        assert_eq!(Seq(u64::MAX).next(), Seq(u64::MAX));
    }

    #[test]
    fn cas_tag_round_trips_without_interpretation() {
        // Including the multipart form, which is not a content hash.
        for raw in [
            "\"abc123\"",
            "\"d41d8cd98f00b204e9800998ecf8427e-3\"",
            "1712345678901234",
        ] {
            assert_eq!(CasTag::new(raw).as_str(), raw);
        }
    }

    #[test]
    fn cas_tags_compare_only_by_equality() {
        assert_eq!(CasTag::new("a"), CasTag::new("a"));
        assert_ne!(CasTag::new("a"), CasTag::new("b"));
    }

    #[test]
    fn cas_failure_distinguishes_lost_from_contended() {
        // The distinction the whole retry policy rests on.
        assert_ne!(CasFailure::Lost, CasFailure::Contended);
    }

    #[test]
    fn identifiers_are_distinct_types() {
        // Compile-time proof that the newtypes do not collapse into one another:
        // this would not compile if TenantId and IndexId were both bare u128.
        let t = TenantId(1);
        let i = IndexId(1);
        assert_eq!(t.0, i.0);
    }
}
