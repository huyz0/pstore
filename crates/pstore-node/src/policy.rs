//! What a node decides, as functions that can be called without a cluster.
//!
//! ⚠️ Everything here was once inside the binary's loop, where `cargo llvm-cov` measured it
//! at 0%. Four real bugs lived in it — a roster that could only ever hold one node's view, a
//! join that could hang forever, a partition with no way back, and a node that could not
//! recognise its own address — and every one was found by running a hundred containers rather
//! than by a test. That is the argument for this module existing.

use crate::ATTEMPT_TIMEOUT;
use pstore_blob::BlobStore;
use pstore_cluster::{Placement, Roster};
use std::time::Duration;

/// How many times a joining node retries the roster before giving up.
const ATTEMPTS: u32 = 12;

/// The longest a single retry may wait, however the backoff arithmetic comes out.
///
/// Comfortably above the real schedule's ceiling (8s of base plus up to as much jitter), so
/// it never binds in normal operation — it exists so that no arithmetic mistake can turn a
/// backoff into a hang.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Reads the roster, retrying with jittered backoff.
///
/// ⚠️ **A cold fleet starts as a thundering herd.** 100 nodes launched together issue 100
/// GETs and 100 conditional PUTs against **one key** within a second or two. Measured
/// against a 1-CPU MinIO, 56 of 100 nodes exhausted their client's ten retries over 58
/// seconds and **exited** — the fleet stabilised at 44. A node that cannot reach the roster
/// has not learned the fleet is empty; it has learned nothing, and exiting turns a slow
/// backstop into a lost member.
///
/// The jitter comes from the node id rather than a clock, so it is reproducible.
pub async fn read_roster_patiently<S: BlobStore>(
    store: &S,
    cluster: &str,
    node_id: &str,
) -> Result<(Roster, Option<pstore_types::CasTag>), String> {
    let mut last = String::new();
    for attempt in 0..ATTEMPTS {
        // ⚠️ Bounded, because a blob request that never returns is worse than one that
        // fails. Measured: 64 of 100 nodes joining at once sat inside a single GET for
        // minutes — zero CPU, no log line, container `running`. A node hung before its
        // first `println!` is indistinguishable from a node that never started, and the
        // retry budget below never ran at all, because the attempt it was protecting had
        // not returned. A timeout is what turns a hang into an error the backoff can see.
        let attempt_result = tokio::time::timeout(ATTEMPT_TIMEOUT, Roster::read(store, cluster))
            .await
            .map_err(|_| format!("no answer in {ATTEMPT_TIMEOUT:?}"))
            .and_then(|r| r.map_err(|e| e.to_string()));
        match attempt_result {
            Ok(r) => return Ok(r),
            Err(e) => {
                last = e;
                // Exponential, jittered by node id: an unjittered retry re-forms the herd it
                // is backing off from.
                let base = 250u64 << attempt.min(5);
                let jitter = jitter(node_id, base.max(1));
                // ⚠️ Clamped, and `saturating_add` rather than `+`. A retry delay is
                // computed from a shift and a modulus, and an arithmetic slip in either
                // produces not a wrong delay but an unrepresentable one — a node that sleeps
                // for centuries is indistinguishable from a hung one, which is the failure
                // this whole function exists to prevent. Found by mutation testing: mutating
                // the `%` in `jitter` hung the test suite instead of failing it.
                let delay = Duration::from_millis(base.saturating_add(jitter)).min(MAX_BACKOFF);
                tokio::time::sleep(delay).await;
            }
        }
    }
    Err(format!(
        "roster unreadable after {ATTEMPTS} attempts: {last}"
    ))
}

/// The peers the roster knows that this node cannot currently see.
///
/// ⚠️ This is the **partition-healing backstop** D-4 asks the blob store to be. `chitchat`
/// takes its seed list once, at startup: a node whose seeds were all unreachable in that
/// instant never contacts anyone again, and no peer can reach it either, because none knows
/// its address. Measured on a 100-node fleet before this existed: 96 nodes converged while
/// one sat at a one-member view for 493 ticks.
#[must_use]
pub fn to_dial<'a>(known: &'a Roster, view: &[String]) -> Vec<&'a str> {
    known
        .nodes()
        .iter()
        .filter(|n| !view.contains(n))
        .map(String::as_str)
        .collect()
}

/// What this node should write to the roster, or `None` if it would add nothing.
///
/// ⚠️ The **union**, never this node's view alone. `refold` replaces the object, so a node
/// that writes only what it sees caps the roster at one node's view — and a node absent from
/// that view is then dialled by nobody, because nobody holds its address. Measured on 100
/// nodes: gossip settled into disjoint groups and the roster stalled at 45 members while 20
/// nodes sat at `members=1`, with the heal loop running and working.
///
/// ⚠️ The cost is that a departed node never leaves. Accepted and recorded: the roster is a
/// *seed list*, so a stale entry costs one wasted datagram, not correctness, and every
/// membership decision reads gossip's live view rather than this object. Eviction needs a
/// node to be absent from every view for longer than any partition, which is M4d's problem.
///
/// Returning `None` when nothing is new is what keeps a converged fleet's steady-state write
/// rate at **zero** rather than one PUT per node per period — a cost that would scale with
/// nodes and carry no information.
#[must_use]
pub fn union_to_publish(known: &Roster, view: &[String]) -> Option<Roster> {
    let union = known.merged(&Roster::from_nodes(view.iter().cloned()));
    (union.nodes().len() != known.nodes().len()).then_some(union)
}

/// How many of `shards` synthetic shards this node would hold, at replication `r`.
///
/// ⚠️ `me` must be the address **gossip advertises**, not the one the node was configured
/// with. `chitchat` advertises a resolved `SocketAddr`, so peers see `10.0.0.7:7946` while
/// the config says `pstore-n7:7946`, and a node comparing the configured name against its own
/// view never finds itself. Measured: every node reported owning 0 of 1000 shards while
/// holding a 44-member view, which reads as a placement bug and is a naming one.
#[must_use]
pub fn owned_shards(roster: &Roster, me: &str, shards: usize, r: usize) -> usize {
    let p = Placement::new(roster);
    (0..shards)
        .filter(|i| p.place(&format!("idx{i}/s0"), r).contains(&me))
        .count()
}

/// The node's slot within a period, derived from its id.
///
/// ⚠️ Spreads work across the period instead of synchronising the fleet on one key. With no
/// leader — no lease is permitted for correctness — every node acts, so an unjittered period
/// puts the whole fleet on one key at the same instant. Derived from the id rather than a
/// clock, so a run is reproducible.
#[must_use]
pub fn jitter(node_id: &str, period: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in node_id.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h % period.max(1)
}

/// A node identity that is fresh on every start.
///
/// ⚠️ Never derived from IP or hostname. A restarted node must look **cold** — it has no
/// cache and owns nothing — and an identity that survives a restart tells the fleet
/// otherwise.
#[must_use]
pub fn fresh_node_id() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{:032x}", t ^ (u128::from(std::process::id())) << 96)
}
