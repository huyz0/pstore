# M4a — Roster and placement

**Serves:** D-4's roster half (the blob store as seed and partition-healing backstop),
D-5 (Local Rendezvous Hashing, C ≈ 32, with CHBL-style overload skipping).

⚠️ **D-43 is not served** and is not cited: placing on `tenant_id` below a size threshold
needs a size signal this milestone has no source for, and `tenancy-scale-model.md` puts ~98%
of indexes in that regime — so a balance figure measured over index-keyed shards says
nothing about the common case. Carried, and named in M4c.

**Scope.** M4 is split. This is the **pure** half — roster, placement, overload skipping —
which has deterministic criteria and needs no network. Membership gossip, the node binary
and the 100-node Docker fleet are **[M4b](../M4b/SPEC.md)**, whose numbers are all
`provisional` and whose evidence regime is entirely different. Splitting them stops the
deterministic half being gated by a container fleet.

## The numbers this milestone is pinned to

⚠️ **Measured before being written here**, not chosen afterwards — the mechanism M3 added
after a criterion turned out to be satisfiable by picking its own threshold. Simulation at
N=100, R=3, C=32, 100,000 keys.

| Name | Value | Why this value |
|---|---|---|
| Window `C` | **32** | D-5. ⚠️ At N=100 that is a third of the fleet, which is why balance below is looser than the corpus's figure. |
| Replication `R` | **3** | The placement list length; not a durability factor, since nodes own nothing. |
| Balance | max node ≤ **1.25×** mean | Measured **1.205** at C=32. ⚠️ `routing-and-placement.md` credits LRH with "within 10–15% of average"; that is **not reproduced** at this C/N ratio — measured 1.171 at C=64 and 1.360 at C=20, so the claim appears to hold only where C ≪ N. Recorded as a correction. |
| Churn, **bulk** change (±50%) | ≤ **1.5×** the theoretical minimum | Measured 1.28–1.36 across five independent hash functions — stable, and the case that matters operationally. The minimum is `k/max(N,N')`, the share the changed nodes must own. |
| Churn, **single node** added | ≤ **3%** of slots, absolute | ⚠️ **Corrected from "≤1.6× the minimum", which was one sample of a noisy statistic pinned as if it were a property.** Across 20 different added-node ids the ratio spans 1.47–2.10× (mean 1.89), and across five strong hashes 1.38–1.95× — because the floor is 1% and the ratio magnifies ring-position luck. The absolute figure is ~1.9% and stable. This is a mis-measurement corrected, **not** a threshold weakened to make a check pass. |
| Churn floor | new nodes receive ≥ **0.8×** their share | ⚠️ Two-sided on purpose: "moves at most X" is satisfied by **0%**, which is exactly what a node that reads the roster and never rebuilds its ring would score. |

## Delta

**Adds** — `pstore-cluster` (layer 3; depends on `pstore-blob` for the roster and nothing
above it, which `Cargo.toml` audits):
- **Roster**: `{h}/clu/ROSTER`, read in **one GET**, refolded by **CAS on a known tag**.
  ⚠️ Not create-if-absent: MinIO ignores the `If-None-Match: *` wildcard — measured, and
  recorded in `Capabilities` — so a cold cluster's first roster write is exactly the race
  that primitive does not survive there. Correctness is asserted against the fault-injecting
  store, never an emulator.
- **Placement**: `placements(key, R) -> [node_id; R]` by LRH over a C-node window.
- **Overload skipping**: step past a node marked hot, relaxing until R are found — *never
  fail*, per `routing-and-placement.md` step 5.

**Does not add**
- **Gossip, the node binary, the Docker fleet.** [M4b](../M4b/SPEC.md).
- **Caching and the post-scale-out cache dip.** M4c — and it is what "zero data movement"
  actually costs, so it is where that claim becomes observable.
- **Per-AZ cells and gray failure.** M4d.

## Acceptance criteria

1. **A roster read costs one GET and zero LIST**, asserted by the request counter.
2. **The roster is refolded by CAS on an observed tag**, and a lost CAS is retried against
   the newer roster rather than overwriting it — asserted with the fault-injecting store,
   which is the only place `Lost` is distinguishable from `Contended`.
3. **Refold contention is bounded**: 100 nodes refolding concurrently issue **≤ 210 blob
   requests** in total (a read and a write each, plus retries), not one per node per retry.
   ⚠️ M4's first draft budgeted "1 CAS per cluster, by whichever node wins" and ignored the
   99 losers; with no leader permitted, every node attempts.
4. **Placement is a pure function of the roster**: the same roster and key give the same
   nodes **in a separate process**, checked against a committed golden vector. ⚠️ Comparing
   two instances in one process cannot catch a process-wide static seed, which is what "seeded
   from a PID" means.
5. **Balance**: over 100,000 keys on 100 nodes, no node holds more than **1.25×** the mean.
6. **Churn is two-sided** on 100→101, 100→150 and 150→100: replica slots reassigned are
   **≤ 1.6×** the theoretical minimum, **and** the added nodes receive **≥ 0.8×** their share.
7. **Nothing is copied.** A fleet change issues **zero blob requests attributable to
   rebalancing** — asserted as a total against the counter. ⚠️ This is what M4's "add/remove
   50% of the fleet with zero data movement" means: `routing-and-placement.md` says ~50% of
   keys remap at 1,000→2,000 *and* that nothing is copied. Placement churn is large and
   data movement is zero; an earlier draft of this spec conflated them and asserted a churn
   bound **below the arithmetic floor**.
8. **Overload skipping sheds only the hot node's share**: marking one node hot reassigns its
   placements and leaves every other placement identical.
9. **Skipping never fails**: with every node in the window marked hot, placement still
   returns R nodes.
10. Region coverage ≥95% on shipped crates, mutation ≥80%, full gate set green.

## Test plan

| # | Test that must fail first | Mutation it catches |
|---|---|---|
| 1 | `reading_the_roster_costs_one_get_and_no_list` | discovery by prefix listing, priced like a PUT and capped at 1000 keys |
| 2 | `a_lost_refold_rebases_rather_than_overwriting` | a refold that clobbers a newer roster, losing members |
| 3 | `a_hundred_concurrent_refolds_are_bounded` | unbounded retry, which multiplies the contention it is meant to survive |
| 4 | `placement_matches_a_golden_vector_in_a_fresh_process` | a placement seeded from a clock, PID, or hash-map iteration order |
| 5 | `placement_is_balanced_at_c_thirty_two` | a hash that clusters, so one node owns a multiple of its share |
| 6 | `growth_moves_no_more_than_the_minimum` | plain modular hashing, which reshuffles everything |
| 6 | `growth_moves_no_less_than_the_new_nodes_share` | **a ring that is never rebuilt** — scores 0% churn and looks perfect |
| 7 | `a_fleet_change_copies_nothing` | a rebalance path, which this architecture must not have |
| 8 | `an_overloaded_node_sheds_only_its_own_shards` | skipping that reshuffles the ring rather than stepping past one node |
| 9 | `skipping_never_returns_fewer_than_r` | a window exhaustion that returns a short list, silently under-replicating |

## RA budget

| Operation | Budget |
|---|---|
| Roster read (join, or refresh) | **1 GET**, 0 LIST |
| Roster refold | 1 GET + 1 conditional PUT per attempting node, **≤ 210 total across 100 nodes** |
| Placement of any key | **0 blob requests** — a pure function of the roster |
| Fleet change | **0 blob requests** — nothing is copied |

## Risks

- **The corpus's balance figure is not reproduced at this C/N ratio**, and the pinned 1.25×
  is measured rather than sourced. If C ≪ N restores 10–15%, the criterion should tighten
  when M4b runs at larger N — in the strengthening direction only.
- **Churn is measured in simulation, not against a running fleet.** The placement function is
  pure, so the simulation *is* the function; what it cannot show is a fleet whose nodes
  disagree about the roster, which is M4b's problem.
- **`R = 3` is a placement-list length, not durability.** Nothing here should be read as a
  replication factor; the blob store is the durable tier.

## Tasks

| ID | Task |
|---|---|
| M4a.1 | `pstore-cluster`: the roster object, one GET, CAS refold on an observed tag |
| M4a.2 | Local Rendezvous Hashing, pure, with a golden vector |
| M4a.3 | CHBL-style overload skipping, including window exhaustion |
| M4a.4 | Churn and balance harness: the numbers above, as a runnable measurement |
