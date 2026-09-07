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
//! ## What this is not
//!
//! Eviction here is a plain LRU over bytes. **D-21 requires class-aware admission** so that a
//! burst of bulk traffic cannot evict the centroid table every query needs, and this does not
//! implement it. Shipping this and calling caching done would leave exactly the failure D-21
//! was written to prevent.

#![forbid(unsafe_code)]

mod cache;

pub use cache::Caching;
