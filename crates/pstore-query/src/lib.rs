//! The query request shape: `prefetch[]` legs, and a `fusion` that combines them.
//!
//! ⚠️ **This crate exists because of D-73, not because of RRF.** *"The API is the hardest
//! thing to change later — harder than the format, because customers' code depends on it."*
//! The shape ships with a retriever it does not have — [`Prefetch::Text`] — and that
//! retriever is **refused by name**. A `prefetch` entry silently dropped is worse than an
//! error: the caller gets a plausible ranking computed from half the retrievers they asked
//! for, and nothing anywhere says so.
//!
//! The second failure this crate is shaped against is arithmetic disguised as concurrency.
//! Two legs awaited in sequence return the same rows as two legs joined, at twice the depth,
//! and every functional test passes. `store.rs` makes the same point about ranges: width is
//! free, depth is not.

mod fuse;
mod run;

pub use fuse::{Fusion, Hit, fuse};
pub use run::{Prefetch, QueryError, Target, query};
