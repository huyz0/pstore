//! Membership by probe, not by gossiped heartbeat.
//!
//! ⚠️ **Why this exists rather than a dependency.** M4b measured `chitchat` at a constant
//! ~13 KB per gossip round at 100 nodes, whatever the period — a converged cluster paid
//! exactly what a churning one did. The fix is to exchange a checksum first and skip the rest
//! when two nodes already agree, and that **cannot be bolted onto Scuttlebutt**: its
//! `NodeDigest` carries a heartbeat and every node increments its own once per round, so the
//! cluster state never reaches a fixed point and a checksum could never match.
//!
//! Liveness therefore comes from **message arrival** — a probe and its ack — and gossiped
//! state carries only what actually changed. That is SWIM, which is what D-4 specified before
//! a licence sent us to `chitchat` instead.
//!
//! ⚠️ **No key-value store.** `chitchat` is a Scuttlebutt KV store whose membership is a side
//! effect; nothing here stores a key. That is the difference that makes writing this a
//! milestone rather than a quarter.

#![forbid(unsafe_code)]

mod cluster;
mod protocol;
mod wire;

pub use cluster::{Cluster, Member, NodeId, State};
pub use protocol::Protocol;
pub use wire::Message;
