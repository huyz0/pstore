# M4c — Membership that scales

**Serves:** [C-7](../../research/04-cluster/membership.md) and M4b's cost measurements, and
finally **D-4 itself**, which specified SWIM+Lifeguard and did not get it.

**Depends on:** [M4a](../M4a/SPEC.md) (roster, rendezvous hashing) and
[M4b](../M4b/SPEC.md) (the fleet harness, and every number this has to beat).

⚠️ **Renumbered.** Caching and the post-scale-out dip, previously M4c, becomes **M4d**;
per-AZ cells and gray failure become **M4e**. Membership comes first: a cache benchmarked on
a fleet whose membership consumes the host measures the host.

## Why a checksum cannot be bolted onto what we have

M4b measured a constant **~13 KB per gossip round at 100 nodes, whatever the period** — a
converged cluster pays exactly what a churning one does. The obvious fix is to exchange a
checksum first and skip the digest when two nodes already agree. **It cannot be done to
`chitchat`**, and the reason is structural rather than an oversight:

* `NodeDigest` carries a `heartbeat` (`digest.rs:11`), and
* `update_self_heartbeat()` runs once per gossip round (`server.rs:323`).

So every node's state changes every round, the cluster state never reaches a fixed point, and
**a checksum would never match**. That is inherent to Scuttlebutt using gossiped heartbeat
counters as its liveness signal. Making a checksum useful means taking liveness *out* of
gossiped state and getting it from **message arrival** instead — direct probes.

Which is SWIM. ⚠️ D-4 specified SWIM+Lifeguard from the start; M4b used `chitchat` only
because `foca` is MPL-2.0 and outside `deny.toml`'s allow-list. So this milestone is not a
detour into a new protocol — it is the specified one, arrived at by measurement.

## Scope: written, not adopted — and the argument against it stated first

M4b's spec said *"writing SWIM to test SWIM is how a milestone becomes a quarter"*, and that
judgement was right for what it described. What is proposed here is smaller, and the
difference is the whole justification:

* **Membership only, no key-value store.** `chitchat` is a Scuttlebutt KV store whose
  membership is a side effect; this node sets **zero keys**. We use perhaps 5% of its surface.
* **No Lifeguard.** Its self-awareness heuristics are a refinement, not a prerequisite, and
  OQ-12 is re-asked rather than assumed (criterion 7).
* **The change we need is at the centre of the wire protocol**, so a fork is not a patch at
  the edge — it is a fork of the protocol, maintained forever, against a crate whose main
  feature we do not use.

⚠️ **This remains the riskiest thing in the milestone.** We lose `chitchat`'s production
exposure at Quickwit, and a membership bug is the kind that looks like everything else — five
of them in M4b did. Criteria 1–3 exist because of that, and the fault-injection harness from
M4b.5 is reused rather than rewritten.

## Phase A — `pstore-gossip`

**Liveness by probe.** Each period a node probes one random peer and waits for an ack. No ack
→ `k` indirect probes through other peers → suspect → dead after a timeout. **O(1) messages
per node per period, independent of N.**

**State is only what changed.** Joins, suspicions, deaths and incarnation refutations, and
nothing else. A stable cluster has genuinely stable state, which is what makes the next line
possible.

**Anti-entropy, checksum-first.** Periodically two nodes exchange an 8-byte checksum over the
member set. Equal — the overwhelmingly common case — and they stop. Different, and they
reconcile. The checksum is maintained **incrementally**, so reading it is O(1) rather than a
re-hash of every member.

## Phase B — hierarchy, and it is GATED

⚠️ **Do not build this until Phase A is measured.** The original argument for hierarchy was
that per-node gossip cost is linear in N. If Phase A delivers O(1) probes and a stable cluster
that exchanges 8 bytes, **that argument is gone**, and building groups and delegates anyway
would be adding a tier to solve a problem that no longer exists.

What would still justify it, on evidence rather than assumption:

* **Cross-AZ traffic is billed** at $0.02/GB round trip ([`az-topology.md`](../../research/04-cluster/az-topology.md)). Zone-sharding is a *cost* argument that survives any protocol change — but it is about which peers you pick, not about tiers of nodes.
* **Blast radius.** A suspicion storm confined to a group is smaller than one that is not.

If Phase A misses criterion 4 at 1,000 nodes, Phase B is specified as it stood: groups of
`clamp(√N, 32, 256)` derived from the roster by rendezvous hash, with **delegates DERIVED,
never elected** — every node reads the same roster, so every node computes the same
representatives, with no ballots, no terms, no election traffic, and a dead delegate's
successor computed by everyone in the same instant. That is what keeps *never add a leader
election for correctness* intact.

## Acceptance criteria

1. **Wire format round-trips, and refuses garbage.** Every message type encodes and decodes to
   an equal value, and a corrupted frame is an error rather than a wrong member set.
2. **A suspicion is refutable.** A node wrongly suspected must refute with a higher
   incarnation and return to alive on every peer — observed to fail against a build that
   ignores incarnation, since without that a false suspicion is permanent.
3. **Convergence, 100 nodes, ≤ 8 periods** — the same bound M4b's `chitchat` fleet met, so the
   replacement is held to what it replaces.
4. ⚠️ **A converged cluster's steady-state gossip is O(1), not O(N).** Bytes/s/node on a
   stable fleet at **100 and 1,000** nodes must vary by **< 2×**, and must be **< 1 KB/s** at
   both — against `chitchat`'s measured 65.7 KB/s at 100 and 406 KB/s at 1,000. This is the
   criterion the milestone exists for.
5. **Fleet CPU at 1,000 nodes ≤ 1 core, summed across all 1,000** — i.e. ≤ 1 millicore per
   node — against `chitchat`'s ~17 cores total. ⚠️ Stated as a **total** because the question
   is what fraction of a machine membership consumes before a query path gets any, and a
   per-node figure hides that behind a number that looks small at any fleet size.
   `scripts/cluster.sh cpu` prints both.
6. **Dead-node detection ≤ 14 periods with 10% probe loss injected** — M4b's measured number,
   reused so a regression is visible. The `Metered` transport from M4b.5 provides the loss.
7. ⚠️ **OQ-12, re-asked.** A CPU-starved *accuser* must evict nobody. M4b answered this for
   phi-accrual; a different detector is a different answer, and the harness already fails the
   test as `VACUOUS` if the accuser was not actually starved.
8. **A partition heals.** Split the fleet, rejoin it, and every node returns to one member set
   within a stated bound — via gossip, and via the roster backstop when gossip alone cannot.
9. Region coverage ≥95% on shipped crates, mutation ≥80%, full gate set green.

## Test plan

| # | Test that must fail first | Mutation it catches |
|---|---|---|
| 1 | `every_message_round_trips` / `a_corrupt_frame_is_an_error_not_a_member` | a decoder that silently truncates a member list |
| 2 | `a_wrongly_suspected_node_refutes_and_returns` | incarnation ignored, so a false suspicion is permanent |
| 4 | `a_converged_cluster_exchanges_a_checksum_not_a_member_list` | a checksum sent *alongside* the state, saving nothing |
| 4 | `a_changed_cluster_still_reconciles` | a checksum that matches when states differ — silent, permanent divergence |
| 4 | `the_checksum_is_maintained_incrementally` | an O(N) re-hash per round, which moves the cost rather than removing it |
| 6 | `a_dead_node_is_detected_under_loss` | a detector that needs a clean network |
| 8 | `a_healed_partition_converges_to_one_member_set` | two halves that stay split because neither re-probes the other |

## Risks

- ⚠️ **Writing a membership protocol is the riskiest thing here**, and the mitigation is that
  M4b's harness — 1,000 real nodes, injected loss, a starved accuser, a killed group — already
  exists and is what will judge it. A protocol that passes those is not proven, but it is
  better tested than the one it replaces was when adopted.
- **Losing `chitchat` means losing its production exposure.** Accepted deliberately, and the
  reason is that its liveness model is precisely what makes the cost floor unremovable.
- **Phase B may never be built.** That is a success, not an omission, and criterion 4 is what
  decides it.
