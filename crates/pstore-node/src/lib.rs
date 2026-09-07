//! A cluster member's decisions, separated from the process that runs them.
//!
//! ⚠️ **Why there is a library here at all.** The binary is a composition root: it reads the
//! environment, opens a blob store, and loops. None of that is reachable from `cargo test`,
//! and while the node's real decisions — when to dial a peer, what to publish to the roster,
//! how long to keep retrying a join — lived inside `main`, they were measured at **0%
//! coverage** and every one of them was a bug waiting to be found by a hundred containers
//! instead of by a test. Two of them already were.
//!
//! So the rule this crate follows: **`main.rs` wires, `lib.rs` decides.** Anything with a
//! branch worth being wrong about belongs here.

use std::time::Duration;

pub mod gossip;
pub mod policy;
pub mod transport;

/// The gossip period a node uses unless told otherwise, and therefore the resolution of
/// every timing it reports.
///
/// ⚠️ **It has to scale with the fleet.** Gossip cost per node is linear in N (M4b criterion
/// 5), so the period that suits 100 nodes asks for roughly 84 cores at 1,000 — and a
/// CPU-starved fleet measures the scheduler, not the protocol. `PSTORE_GOSSIP_PERIOD_MS`
/// overrides it, and every number in a milestone ledger names the period it was taken at.
///
/// ⚠️ **1s, not the 200ms M4b measured at.** Measured at 100 nodes, 200ms costs the fleet
/// 0.60 cores and 1s costs 0.20 — 3x, for a constant that buys nothing but wall-clock. Every
/// criterion is counted in *periods*, so none of them move; only their translation into
/// seconds does, and detection at 14 periods becomes 14s rather than 2.8s. A fleet whose
/// nodes own nothing tolerates that: a stale liveness view costs a retry, never a wrong
/// answer.
///
/// ⚠️ Whatever the value, ONE value: it is `chitchat`'s `gossip_interval` and the loop's
/// sampling interval both, passed rather than repeated, because a comment saying two numbers
/// must match is a rule an agent has to remember and the compiler can hold it instead.
pub const DEFAULT_GOSSIP_PERIOD: Duration = Duration::from_secs(1);

/// How long one roster request may take before it counts as failed.
///
/// Generous against a store on the same host, and the point is not latency — it is that an
/// unbounded wait cannot be retried, and a request that cannot be retried cannot be backed
/// off.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a node re-reads the roster to dial peers it cannot see, and publishes any it
/// knows that the roster does not.
///
/// ⚠️ Short, because it is a *read* that usually writes nothing: a converged fleet adds
/// nothing to the union and issues no PUT at all. At 60s a cold 100-node fleet spent minutes
/// in disjoint groups waiting for the first heal.
pub const HEAL_PERIOD: Duration = Duration::from_secs(10);
