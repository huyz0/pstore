//! A read cache keyed by the **requested** range.
//!
//! ## ⚠️ It goes outermost, and that is not a preference
//!
//! No decorator in this workspace overrides `get_ranges` — `Accounted`, `Congested`, `Faulty`
//! and the testkit's five all inherit the trait default, which *coalesces* and then calls
//! `get_range` (`pstore-blob/src/store.rs`). A cache placed underneath therefore never sees a
//! requested range at all, only a merged span whose shape changes with the probe set: two
//! queries touching the same posting list would share nothing, and the cache would look
//! correct while buying almost no hits.
//!
//! So the layering is `Caching<Accounted<_>>` — cache outside, accounting inside. It is also
//! the only order in which "a hit costs zero requests" is observable, because a counter above
//! the cache bills the call before the cache can answer it.
//!
//! ## ⚠️ It refuses to cache `get`
//!
//! What makes a cache entry correct forever is that segments are **immutable**, and that
//! property does not extend to whole-object reads: `pstore-engine`'s lane registry is read
//! with `get` and CAS-mutated in place. Caching it would make a newly registered lane
//! permanently invisible and its bundles unrecoverable — silent data loss, from a cache that
//! passes every hit-rate test there is. `get` therefore passes straight through, and the
//! centroid table (which uses it) stays uncached until an explicit immutability marker exists.
//!
//! ## Two tiers, one core (M20)
//!
//! A [`CacheCore`] holds the state, and every tenant's [`Caching`] view shares it: each key
//! names its tenant, so one budget serves them all. It has a memory tier with a **quota per
//! class** (D-21: a bulk burst evicts only bulk), and optionally a disk tier behind it, one
//! `foyer` instance per class with the same shares (D-22). The disk tier survives a restart
//! (D-23), is emptied when the store behind its directory changes, and degrades to the store
//! when the disk fails. A `Caching` with no core forwards every call verbatim.
//!
//! ## What this is not
//!
//! **Endurance throttling** (`disk-space-management.md` §7) is not configured, and the
//! memory tier's LRU is a `Vec` (BACKLOG row 45). And D-23's "do not scan the directory" is
//! not met: `foyer` reads every block header on open (M20's ledger bounds it).

#![forbid(unsafe_code)]

mod cache;
mod disk;

pub use cache::{CacheCore, Caching, DiskState};
pub use disk::{DEFAULT_BLOCK, DiskConfig};
