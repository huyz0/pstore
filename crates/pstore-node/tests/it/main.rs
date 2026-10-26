//! Every integration test of this crate, in one binary.
//!
//! ⚠️ One binary, not one per file: each `tests/*.rs` is its own crate that compiles and
//! links its own copy of the workspace's generic code, and 148 of them were 95% of an
//! incremental test build (390 s after touching one library file). A new test file goes
//! here as a module. Benches and the tests scripts run by name (`--test <name>`) stay
//! separate binaries beside this directory.

mod fnv;
mod gossip;
mod policy;
mod schedule;
mod swim;
