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
pub mod explain;
mod filter;
mod fuse;
mod order;
mod run;

pub use aggregate::{Aggregate, Aggregator, Key, Spec as AggregateSpec, Total, aggregate};
pub use filter::{
    Bound, ID as ID_ATTRIBUTE, MAX_PATTERN, Needle, Op, Pattern, PatternKind, Predicate, TokenOp,
};
pub use fuse::{Fusion, Hit, MAX_LEGS, Weights, fuse};
pub use order::{Located, MAX_KEYS, Order, OrderBy, Selector, select, select_held, vectors_at};
pub use run::{
    Elsewhere, FieldStats, OpenPart, PartHits, Prefetch, QueryError, Share, SumCut, Target, Texts,
    only, open_part, part, query, query_rows_filtered, query_rows_filtered_texts, query_rows_split,
    query_rows_split_texts, query_with, query_with_texts, scan_part, splittable,
};
