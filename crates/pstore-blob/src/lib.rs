//! The blob substrate: the only way pstore touches durable storage.
//!
//! Nothing above this crate may depend on `object_store` directly — enforced by a
//! `cargo-deny` ban — so request accounting and congestion control cannot be bypassed.
//!
//! See `docs/research/02-object-storage/` for why each primitive is shaped as it is, and
//! `docs/milestones/M0a/SPEC.md` for what this milestone claims.

mod accounting;
mod coalesce;
mod memory;
mod store;
mod types;

pub use accounting::{Accounted, OpClass, TenantView};
pub use coalesce::{Fetch, coalesce};
pub use memory::{MemoryStore, TagStyle};
pub use store::BlobStore;
pub use types::{BlobError, Capabilities, CasError, Key, Precondition, PutOutcome, Support};
