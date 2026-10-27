//! Every integration test of this crate, in one binary.
//!
//! ⚠️ One binary, not one per file: each `tests/*.rs` is its own crate that compiles and
//! links its own copy of the workspace's generic code, and 148 of them were 95% of an
//! incremental test build (390 s after touching one library file). A new test file goes
//! here as a module. Benches and the tests scripts run by name (`--test <name>`) stay
//! separate binaries beside this directory.

mod analyzer;
mod corrupt;
mod external;
mod fde;
mod fields;
mod geo;
mod guards;
mod ids_at;
mod index_rows;
mod lance;
mod lance21;
mod multi_text;
mod parquet;
mod search;
mod sections;
mod segment;
mod sketch;
mod sparse;
mod text;
mod text_fields;
mod typed_values;
mod vector_fields;
