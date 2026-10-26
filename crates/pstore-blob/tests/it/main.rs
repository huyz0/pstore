//! Every integration test of this crate, in one binary.
//!
//! ⚠️ One binary, not one per file: each `tests/*.rs` is its own crate that compiles and
//! links its own copy of the workspace's generic code, and 148 of them were 95% of an
//! incremental test build (390 s after touching one library file). A new test file goes
//! here as a module. Benches and the tests scripts run by name (`--test <name>`) stay
//! separate binaries beside this directory.

mod accounting;
mod batch_delete;
mod capabilities;
mod cas;
mod coalescing;
mod dyn_store;
mod faults;
mod memory_tags;
mod object_store_adapter;
mod scanning;
mod semantics;
mod stat;
