//! The catalog: which tenants exist, answered without ever enumerating a bucket.
//!
//! Three questions have to be answerable at 1M tenants
//! ([`catalog-without-master.md`](../../../docs/research/03-metadata-consistency/catalog-without-master.md)).
//! Two of them — *where is tenant X* and *does it exist* — are hot, and the answer is that
//! **they never touch this crate**: a tenant's HEAD key is derived from its id, so opening a
//! tenant costs one GET at any scale. This crate answers only the third, *what exists*, which
//! is cold, and which must still never cost a LIST.
//!
//! ## The shape
//!
//! ```text
//! cat/root                                     MUTABLE · CAS · {epoch, width}
//! {bucket:04x}/cat/b/HEAD                      MUTABLE · CAS · {run_epoch, digest, pending[]}
//! {bucket:04x}/cat/b/{run_epoch:020}-{digest}  immutable  · the sorted run
//! ```
//!
//! `width` independent registers, and a root that changes only when the width does. Every key
//! is derived, so a full enumeration is `width` GETs issued together, then the runs they name
//! issued together: **two rounds, no LIST, at any tenant count.**
//!
//! ## ⚠️ C-12 — the corpus's change log is not built, and why
//!
//! `catalog-without-master.md` §2 gives each bucket a per-lane change log drained by a
//! background folder, so that a creation does not rewrite a whole bucket. The premise is
//! right; a separate object space is not the only way to honour it. Pending records live in
//! the bucket's own pointer, bounded by [`MAX_PENDING`] rather than by occupancy.
//!
//! The reuse it proposes could not have worked as written: the engine's lanes are
//! **node**-scoped, so their id space is unbounded and a reader cannot learn which lanes exist
//! without a registry object — a second mutable object per bucket *and* a third sequential
//! round in every enumeration. What C-12 costs instead is a CAS on the append path, which is
//! affordable only because an append is a **tenant-lifecycle** event and not a commit;
//! [`Appender::observe`] is what keeps that true.
//!
//! ## What this crate deliberately cannot reach
//!
//! It depends on `pstore-blob` and `pstore-types` and nothing else. Not on `pstore-engine`:
//! the catalog is **derived** state that must be rebuildable without it, and a catalog that
//! *could* read HEAD would eventually be asked to. Nothing in the workspace depends on this
//! crate either, which is how "a tenant open issues zero catalog requests" is enforced —
//! by there being no edge to traverse.

mod append;
mod bucket;
mod enumerate;
mod keys;
mod record;

pub use append::Appender;
pub use bucket::{BucketHead, Root, fold, read_head, read_root, write_root};
pub use enumerate::{Enumeration, Mark, enumerate, enumerate_since};
pub use keys::{DEFAULT_WIDTH, MAX_WIDTH, Width, bucket_of, head_key, root_key, run_key};
pub use record::{State, TenantRecord};

/// Pending records a bucket pointer may carry before an append folds it inline.
///
/// ⚠️ **This is the whole of C-12's cheap side, so the number matters.** At
/// [`DEFAULT_WIDTH`] a bucket holds ~61 records — a ~73 KB run — and eight ~1.2 KB records
/// is ~10 KB. Raise it far and the pointer costs what the run costs, at which point carrying
/// pending in the pointer has bought nothing; that is what
/// `an_observe_moves_fewer_bytes_than_a_fold` exists to notice.
///
/// It also bounds the pointer **structurally**: the append that would exceed the cap folds
/// first, so a folder that never runs is a cost problem and never a correctness one.
pub const MAX_PENDING: usize = 8;

/// How many times a CAS is rebased before giving up.
///
/// Bounded rather than `loop`: the exit condition is "no one else won this round", and a
/// backend that is permanently contended would otherwise spin forever on a path a caller
/// waits on.
const MAX_CAS_ATTEMPTS: u32 = 16;

/// What can go wrong.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The blob store refused.
    #[error("blob store: {0}")]
    Blob(#[from] pstore_blob::BlobError),
    /// An object did not decode. Never silently skipped: a bucket that cannot be read is an
    /// error, because the alternative is an enumeration that quietly under-reports.
    #[error("catalog object at {0} is malformed")]
    Corrupt(String),
    /// The head named a run that is not there. A derived key that 404s is normal; a run named
    /// by a committed pointer is not, and treating it as "empty bucket" is exactly how a
    /// catalog loses records without anyone noticing.
    #[error("bucket {0} names a run that does not exist")]
    MissingRun(u32),
    /// Too many writers kept winning the CAS on a bucket pointer. Retryable.
    #[error("bucket {0} stayed contended for {1} attempts")]
    Contended(u32, u32),
    /// Another writer changed the root. ⚠️ Distinct from [`Self::Contended`] because the root
    /// is not a bucket, and `Contended(0, _)` would be indistinguishable from bucket zero.
    #[error("cat/root changed under this writer")]
    RootContended,
    /// The stored width is not one this key format can express.
    #[error("root names width {0}, which is not in 1..={MAX_WIDTH}")]
    BadWidth(u32),
}
