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

/// The gossip period, and therefore the resolution of every timing a node reports.
///
/// ⚠️ This constant IS `chitchat`'s `gossip_interval` — `gossip::start` reads it rather than
/// repeating the literal, because a comment saying two numbers must match is a rule an agent
/// has to remember, and the compiler can hold it instead. `scripts/cluster.sh`'s
/// `GOSSIP_PERIOD_MS` still has to agree by hand; it converts seconds to periods for
/// reporting only, and gates nothing.
pub const GOSSIP_PERIOD: Duration = Duration::from_millis(200);

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
