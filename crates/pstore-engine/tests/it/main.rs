//! Every integration test of this crate, in one binary.
//!
//! ⚠️ One binary, not one per file: each `tests/*.rs` is its own crate that compiles and
//! links its own copy of the workspace's generic code, and 148 of them were 95% of an
//! incremental test build (390 s after touching one library file). A new test file goes
//! here as a module. Benches and the tests scripts run by name (`--test <name>`) stay
//! separate binaries beside this directory.

mod abandon;
mod accessors;
mod aggregate;
mod analyzer;
mod applied;
mod arithmetic;
mod branch;
mod buffered;
mod bundle_errors;
mod centroid_skip;
mod cold_depth;
mod compaction;
mod create;
mod dense_index;
mod depth_at_scale;
mod dict_skip;
mod dimensions;
mod distance;
mod divergent;
mod drop_index;
mod end_to_end;
mod engine;
mod engine_query;
mod explain;
mod external_copy;
mod external_index;
mod external_retry;
mod failures;
mod fde_skip;
mod filter_rows;
mod filters;
mod fold_overtaken;
mod fresh_query;
mod gc;
mod gc_batch;
mod head;
mod idempotency;
mod idle;
mod invariants;
mod lane_taken;
mod lanes;
mod lengths;
mod linearizability;
mod memtable_races;
mod multi;
mod multi_text;
mod named_dense_clusters;
mod open_cost;
mod oq6;
mod oq91;
mod order;
mod patch;
mod progress;
mod quarantine;
mod read_only;
mod reap;
mod recall;
mod ref_new;
mod refusals;
mod replicate;
mod replication;
mod restart;
mod retry_ceiling;
mod scan_cache;
mod schema;
mod segment_names;
mod served_epoch;
mod sketch;
mod snapshot;
mod sparse;
mod split;
mod text_field;
mod time_travel;
mod times;
mod updated_epoch;
mod upsert;
mod vectors;
mod warm;
