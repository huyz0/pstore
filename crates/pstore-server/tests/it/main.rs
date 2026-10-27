//! Every integration test of this crate, in one binary.
//!
//! ⚠️ One binary, not one per file: each `tests/*.rs` is its own crate that compiles and
//! links its own copy of the workspace's generic code, and 148 of them were 95% of an
//! incremental test build (390 s after touching one library file). A new test file goes
//! here as a module. Benches and the tests scripts run by name (`--test <name>`) stay
//! separate binaries beside this directory.

mod aggregate;
mod allow_partial;
mod analyzer;
mod applied_writes;
mod arrays;
mod attributes;
mod auth_baseline;
mod auth_contract;
mod auth_peers;
mod auth_sources;
mod boost;
mod bounded;
mod branch;
mod chaos;
mod closed_requests;
mod composition;
mod consistency;
mod cost;
mod datetime;
mod deploy;
mod depth;
mod distance;
mod diversify;
mod door;
mod durable;
mod errors;
mod external;
mod facets;
mod filters;
mod geo;
mod highlight;
mod idempotency;
mod idle;
mod lane_taken;
mod lifecycle;
mod metrics;
mod multi;
mod multi_fde;
mod named_dense;
mod order;
mod order_keys;
mod patch;
mod patch_by_filter;
mod patterns;
#[path = "../support/peer.rs"]
mod peer_fixture;
mod peer_startup;
mod peers;
mod quarantine;
mod read_cache;
mod ref_new;
mod registry;
mod replication;
mod replication_races;
mod rerank;
mod restart;
mod scheduled_fold;
mod scheduled_reap;
mod schema;
mod served_epoch;
mod session;
mod sketch;
mod snapshot;
mod socket;
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test support: the lints its including files allowed for it as crate roots"
)]
#[path = "../support/mod.rs"]
mod support;
mod text_arrays;
mod time_travel;
#[allow(
    dead_code,
    clippy::duplicate_mod,
    reason = "peer.rs includes tls.rs privately as well; auth_peers uses it directly"
)]
#[path = "../support/tls.rs"]
mod tls_fixture;
mod tokens;
mod typed_values;
mod upsert;
mod warm;
