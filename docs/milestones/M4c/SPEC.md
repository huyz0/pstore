# M4c — Hierarchical membership

**Serves:** [C-7](../../research/04-cluster/membership.md), which stopped being an argument
and became a measurement in M4b: flat gossip costs **406 KB/s and ~17 of 20 cores at 1,000
nodes**, for membership alone, with no index, no query path and no data. `membership.md` calls
zone-sharded gossip *required, not optional*; this is that, specified.

**Depends on:** [M4a](../M4a/SPEC.md) (the roster, and rendezvous hashing) and
[M4b](../M4b/SPEC.md) (the gossip mesh this makes into one tier of two).

⚠️ **Renumbered.** Caching and the post-scale-out dip, previously M4c, becomes **M4d**;
per-AZ cells and gray failure become **M4e**. Membership has to come first: a cache measured
on a fleet whose membership consumes the host measures the host.

## The shape

Three layers, of which only two are gossip.

| Layer | Carries | Cost per node | Where it lives |
|---|---|---|---|
| **Roster** — blob store | Full membership: who *exists* | 1 conditional GET per heal period; **304, no body**, in steady state | M4a, exists |
| **Group mesh** | Full liveness within a group of `G` | **O(G)** — independent of fleet size | this milestone |
| **Delegate mesh** | Per-group *summaries*, never per-node state | paid only by delegates | this milestone |

**The separation that makes it cheap.** Flat gossip conflates two things:

* **Membership** — who exists. Every node needs all of it, because placement is a pure
  function of the roster and every node must compute the same answer. But it is a
  *document*, not a conversation: it changes on join and departure, not continuously, and the
  blob store already holds it. O(N) *state*, O(1) steady-state *bytes*.
* **Liveness** — who is up right now. Latency-sensitive, and the only reason gossip exists
  here. It needs full fidelity **only for nodes this node actually talks to**.

Hierarchy is affordable precisely because coarse cross-group liveness is enough: nodes own
nothing, so a stale liveness view costs one wasted request and a retry elsewhere — never a
wrong answer. `membership.md` already states this ("membership is only an optimization");
this milestone is the first thing to depend on it.

## ⚠️ Delegates are DERIVED, never elected

The obvious design is an election: each group votes for representatives. **This does not, and
must not.**

Every node already reads the same roster. So every node can *compute* the same delegates from
it, by rendezvous hash on `(group_id, node_id)` — the machinery in `pstore-cluster::Placement`,
unchanged. No ballots, no terms, no election messages, no convergence delay, and when a
delegate dies every node independently computes the same successor **in the same instant**,
because they are reading the same document.

This is what keeps the non-negotiable intact: *never add a lock, lease, or leader election for
correctness*. Delegates are a **propagation optimization**. Correctness never depends on one
existing, and criterion 6 is the test that says so — kill every delegate of a group and the
fleet must still reach correct membership through the roster.

## Delta

**Adds**
- `pstore-cluster`: `groups(roster, g) -> Vec<Group>` and `delegates(roster, group, k)`, both
  pure functions of the roster, both built on the existing rendezvous hash.
- `pstore-node`: two `chitchat` instances instead of one — a group mesh and, on delegates
  only, a delegate mesh. Regrouping when the roster version changes.
- `scripts/cluster.sh`: `--hierarchical`, so the flat mesh stays runnable and every number
  below has a control.

- **Checksum-first reconciliation.** ⚠️ Orthogonal to hierarchy, and measured to matter more
  in steady state. chitchat's `Syn` carries a full `Digest` — a per-node version map — **every
  round, whether or not anything changed**: at 100 nodes that is a constant **~13 KB per
  round** regardless of period (M4b, "Where the CPU actually goes"). Two nodes whose state
  already agrees should exchange a **checksum of the cluster state** and stop, paying O(1)
  instead of O(N).

  Hierarchy bounds how cost *grows*; this bounds what it costs to *sit still*, and a fleet is
  converged almost all of the time. It needs a change to the wire protocol, so the options are
  an upstream contribution to `chitchat` or a fork — named here as a decision, not assumed.

**Does not add** — a third tier (needed only past ~100k nodes; `G = √N` keeps both meshes at
√N up to that point), cross-AZ relay accounting (M4e), caching (M4d).

## Group size

`G = clamp(round(√N), 32, 256)`, and the default is `√N` rather than a constant because it
balances the two meshes against each other: group size `√N` and group *count* `√N` means
neither tier grows faster than the other. A fixed `G` makes the delegate mesh O(N) and simply
moves the bottleneck. Clamped below so a small fleet does not fragment into groups of three,
and above so one group cannot grow back into the thing being fixed.

At N=1,000 this gives **G=32, ~31 groups, a 93-member delegate mesh at k=3** — so the
prediction criterion 3 has to meet is that per-node cost lands near M4b's measured 25-node
figure of 22.8 KB/s, not its 1,000-node figure of 406 KB/s.

## Acceptance criteria

1. **Grouping is a pure function of the roster**: the same roster gives the same groups and
   the same delegates **in a separate process**, against a committed golden vector. ⚠️ Same
   reason as [M4a](../M4a/VERIFIED.md) criterion 4 — two instances in one process share a
   seed and agree regardless.
2. **Placement is unchanged.** A node's computed placement must be **bit-identical** to
   M4a's for every key, and must not depend on which group the node is in. Asserted against
   M4a's existing golden vector. ⚠️ Without this, hierarchy can silently partition placement
   and every node still answers plausibly.
3. **Per-node gossip cost is independent of fleet size**: bytes/s/node at **250, 500 and
   1,000** nodes varies by **< 1.5×** (all three share `G=32`), against the flat mesh's
   **5.4×** over 100→1,000. Measured with `scripts/cluster.sh traffic`, flat mesh as control.
4. **The 1,000-node fleet fits the host**: total fleet CPU **≤ 5 cores** and host load
   average **< 100**, against the flat mesh's ~17 cores at load 1162. ⚠️ Stated as an
   absolute because the point is not a ratio — it is whether the machine can also run a
   query path.
5. **Intra-group convergence** ≤ **8 periods**, the same bound M4b's 100-node fleet met, since
   a group is a flat mesh of `G`.
6. ⚠️ **Losing every delegate of a group degrades, never breaks.** Kill all `k` delegates of
   one group; every surviving node must still reach correct membership within **≤ 3 heal
   periods**, via the roster. This is the criterion that proves delegates are an optimization
   rather than a control plane, and it must be observed to fail against a build whose roster
   backstop is disabled.
7. **Cross-group death detection** within **≤ 40 periods** with 10% probe loss injected —
   honestly larger than M4b's 14, because it is three gossip hops instead of one. The number
   is in the criterion so that a regression is visible rather than absorbed.
8. **Group balance**: no group larger than **1.25×** the mean, at 1,000 nodes.
9. ⚠️ **A converged cluster's gossip is O(1) per round, not O(N).** With checksum-first
   reconciliation, bytes per round on a **stable** 100-node group must be **< 1 KB**, against
   the ~13 KB measured for chitchat as it stands — and must not grow when the group does:
   measured at group sizes 32 and 100, the per-round bytes vary by **< 2×**. ⚠️ This is
   separately falsifiable from criterion 3, which a hierarchy alone can satisfy while still
   paying O(N) inside each group.
10. **The steady-state saving does not cost detection.** With checksum-first in place,
    criterion 7's cross-group detection bound must still hold at the **200ms** period — the
    point of making a stable cluster cheap is to *avoid* having to slow gossip down. ⚠️ Stated
    because the cheap alternative is simply raising the period, which M4b showed reaches
    0.11 cores at 100 nodes with no code at all, and pays for it in seconds of detection
    latency.
11. Region coverage ≥95% on shipped crates, mutation ≥80%, full gate set green.

## Test plan

| # | Test that must fail first | Mutation it catches |
|---|---|---|
| 1 | `groups_match_a_golden_vector_in_a_fresh_process` | grouping seeded from a clock, PID, or map iteration order |
| 2 | `hierarchy_does_not_change_placement` | a placement that silently becomes group-local |
| 3 | `per_node_gossip_does_not_grow_with_the_fleet` | a "hierarchy" that still floods every node |
| 6 | `a_group_survives_losing_every_delegate` | delegates becoming a correctness dependency — a control plane by accident |
| 6 | `the_roster_backstop_is_what_saves_it` | the above passing because gossip healed it anyway, which would prove nothing |
| 8 | `groups_are_balanced_at_a_thousand_nodes` | a hash that clusters, so one group is the whole fleet |
| 9 | `a_converged_group_exchanges_a_checksum_not_a_digest` | a "checksum" that is sent *alongside* the digest, saving nothing |
| 9 | `a_changed_group_still_reconciles` | a checksum that matches when the states differ — silent, permanent divergence |

## Risks

- **Two meshes is two failure modes.** A node in the wrong group is invisible to the right
  one; criterion 1's golden vector is what keeps grouping from drifting per-node.
- **Regrouping churn.** A roster change moves nodes between groups, and a naive
  implementation rebuilds both meshes on every change. Rendezvous hashing bounds the churn —
  the same property M4a measured for placement — but it has to be measured here too.
- ⚠️ **`G = √N` means per-node cost is O(√N), not O(1).** Honest about it: at 1M nodes a
  group is 1,000 and the fix is a third tier. Out of scope, and named so it is not discovered
  later as a surprise.
