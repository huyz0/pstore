//! Conformance probes and sensitivity sweeps.
//!
//! Never shipped: a `dev-dependencies` crate. See `docs/milestones/M0a/SPEC.md`.

pub mod audit;
pub mod broken;
pub mod budget;
pub mod claims;
pub mod conformance;
pub mod depth;
#[cfg(feature = "azure")]
pub mod fake_azure;
#[cfg(feature = "s3")]
pub mod fake_s3;
pub mod flaky;
pub mod gated;
pub mod no_suffix;
pub mod racy;
pub mod sim;
pub mod sweep;
