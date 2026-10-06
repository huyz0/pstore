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

mod aggregate;
pub mod condition;
pub mod deletes;
mod filter;
mod fuse;
mod order;
mod run;

pub use aggregate::{Aggregate, Aggregator, Key, Spec as AggregateSpec, Total, aggregate};
pub use filter::{
    Bound, ID as ID_ATTRIBUTE, MAX_PATTERN, Op, Pattern, PatternKind, Predicate, TokenOp,
};
pub use fuse::{Fusion, Hit, MAX_LEGS, Weights, fuse};
pub use order::{OrderBy, Selector, select};
pub use run::{
    Elsewhere, OpenPart, PartHits, Prefetch, QueryError, Share, Target, only, open_part, part,
    query, query_rows_filtered, query_rows_split, query_with, scan_part, splittable,
};
