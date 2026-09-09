//! The vocabulary of the blob layer.

use pstore_types::CasTag;
use std::fmt;

/// A blob key. Always **derived** from facts the caller already holds — never discovered
/// by listing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(String);

impl Key {
    /// Wraps a derived key.
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The key as a string, for the backend.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The condition a conditional write is made under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    /// Create only if absent. Maps to `If-None-Match: *` / `ifGenerationMatch=0`.
    ///
    /// ⚠️ The least portable of the primitives: MinIO rejects the `*` wildcard outright.
    NotExists,
    /// Replace only if the current tag matches. Maps to `If-Match` / `ifGenerationMatch`.
    Match(CasTag),
}

/// Why a conditional write did not land.
///
/// `Lost` and `Contended` are distinct because conflating S3's 412 and 409 causes rebase
/// storms: 412 means another writer won and we must rebase; 409 means the backend could
/// not evaluate the condition and we should retry the same attempt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CasError {
    /// HTTP 412. Another writer won. **Rebase, then retry.**
    #[error("precondition failed: another writer won")]
    Lost,
    /// HTTP 409. The backend could not evaluate the condition. **Retry without rebasing.**
    #[error("conditional request conflict: retry without rebasing")]
    Contended,
    /// Anything else.
    #[error("blob store error: {0}")]
    Io(String),
}

impl CasError {
    /// Whether losing this way invalidates the state the caller built its attempt on.
    ///
    /// Getting this wrong is expensive in exactly one direction: treating `Contended` as
    /// `Lost` rebuilds the world on every transient conflict.
    #[must_use]
    pub fn should_rebase(&self) -> bool {
        matches!(self, Self::Lost)
    }
}

/// Anything that can go wrong on an unconditional operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobError {
    /// The key does not exist. On the WAL tail-probe path this is a **normal** answer:
    /// it is what bounds the lane.
    #[error("no such key: {0}")]
    NotFound(String),
    /// The backend asked us to slow down. A normal signal, not an error.
    #[error("slow down")]
    SlowDown,
    /// The requested range is not inside the object.
    #[error("range {0:?} outside object of {1} bytes")]
    RangeOutOfBounds(std::ops::Range<u64>, u64),
    /// Anything else.
    #[error("blob store error: {0}")]
    Other(String),
}

/// What a write returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutOutcome {
    /// The tag to condition the next write on.
    pub tag: CasTag,
}

/// What a backend can actually do.
///
/// ⚠️ **Measured, not declared.** The conformance suite populates this from observed
/// behaviour, because self-hosted implementations diverge on precisely the CAS semantics
/// we depend on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Human-readable backend name, for the recorded profile.
    pub backend: String,
    /// `If-Match`-style compare-and-swap.
    pub cas: Support,
    /// `If-None-Match: *`-style create-if-absent. The least portable primitive.
    pub create_if_absent: Support,
    /// Whether DELETE is billed. Free on S3, billed on GCS and Azure, so GC policy differs.
    pub delete_is_free: bool,
    /// Largest batch the backend accepts in one delete request.
    pub max_batch_delete: usize,
    /// `G*`: merge two ranged reads when the gap between them is smaller than this.
    ///
    /// Backend-tuned, because it is where `bandwidth x latency_saved` equals the cost of
    /// one more request. Zero disables merging.
    pub coalesce_gap: u64,
}

impl Capabilities {
    /// Whether this backend may be trusted with a write a caller will be told is durable.
    ///
    /// ⚠️ **`Supported` on both primitives, or nothing.** `Divergent` is the dangerous
    /// answer, not the safe one: it means the primitive is *present* — the call returns
    /// success — and wrong, which is precisely how MinIO accepts `If-None-Match: *` and
    /// then lets a second create through. Reading "present" as "usable" is the failure this
    /// predicate exists to make impossible, so the check is on `Supported` itself rather
    /// than on the absence of `Unsupported`.
    ///
    /// `create_if_absent` counts as well as `cas`, because the two are used for different
    /// things and losing either loses correctness: `cas` fences a committer against a world
    /// that moved, `create_if_absent` is what makes a create idempotent and a lane object
    /// write-once.
    #[must_use]
    pub fn admits_durable_writes(&self) -> bool {
        self.cas == Support::Supported && self.create_if_absent == Support::Supported
    }

    /// Which primitive is not `Supported`, for an error a reader can act on.
    #[must_use]
    pub fn first_divergence(&self) -> Option<(&'static str, &Support)> {
        if self.cas != Support::Supported {
            Some(("compare_and_swap", &self.cas))
        } else if self.create_if_absent != Support::Supported {
            Some(("create_if_absent", &self.create_if_absent))
        } else {
            None
        }
    }
}

/// How well a backend supports a primitive, as observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    /// Behaves as the specification requires.
    Supported,
    /// Present but wrong in a way that matters. Carries what was observed.
    ///
    /// A backend recorded divergent here **must not serve `durable` writes** — failing
    /// loudly at startup beats corrupting silently.
    Divergent(String),
    /// Not offered at all.
    Unsupported,
}
