//! The blob substrate: the only way pstore touches durable storage.
//!
//! Nothing above this crate may depend on `object_store` directly — enforced by a
//! `cargo-deny` ban — so request accounting and congestion control cannot be bypassed.
//!
//! See `docs/research/02-object-storage/` for why each primitive is shaped as it is, and
//! `docs/milestones/M0a/SPEC.md` for what this milestone claims.

mod accounting;
mod coalesce;
mod congestion;
mod faulty;
mod memory;
#[cfg(feature = "object_store")]
mod object_store_backend;
mod store;
mod types;

pub use accounting::{Accounted, OpClass, TenantView};
pub use coalesce::{Fetch, coalesce};
pub use congestion::Congested;
pub use faulty::{Faults, Faulty};
pub use memory::{MemoryStore, TagStyle};
#[cfg(feature = "object_store")]
pub use object_store_backend::ObjectStoreBackend;
pub use store::BlobStore;
pub use types::{BlobError, Capabilities, CasError, Key, Precondition, PutOutcome, Support};
