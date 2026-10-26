//! Every integration test of this crate, in one binary.
//!
//! ⚠️ One binary, not one per file: each `tests/*.rs` is its own crate that compiles and
//! links its own copy of the workspace's generic code, and 148 of them were 95% of an
//! incremental test build (390 s after touching one library file). A new test file goes
//! here as a module. Benches and the tests scripts run by name (`--test <name>`) stay
//! separate binaries beside this directory.

mod clustering;
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test support: the lints its including files allowed for it as crate roots"
)]
#[path = "../common/mod.rs"]
mod common;
mod exq;
mod external;
mod fde_clusters;
mod ladder;
mod lire;
mod persisted;
mod probe_sq8;
mod rabitq;
mod recall;
mod rung0;
mod sparse;
mod text;
mod warming;
