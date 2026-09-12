# M4e — Verified (phase 1)

One line per acceptance criterion in [SPEC.md](SPEC.md), naming the test or command that
demonstrated it.

Gate: `scripts/check-verified.py`.

⚠️ **All three phases are done.** What is deliberately **not** built: the cross-AZ probe mesh
(D-82) and the blob health bulletin (D-83) as running subsystems. They are transports for the
phase-3 decisions and need a query path and a real multi-AZ deployment to mean anything;
neither exists. The decisions are what can be built now, and they are the dangerous part.

1. ⚠️ **Cell addresses do not collide**, over 10,000 generated `(cluster, zone)` pairs —
   `cell_keys_do_not_collide_across_ten_thousand_cells`,
   `a_cell_key_names_both_its_cluster_and_its_zone`
   (`cargo test -p pstore-cluster --test cell`). Observed to fail against the previous key:
   *"cluster-0/az-1 and cluster-0/az-0 share the roster key c430/clu/ROSTER"*.
   Also `the_key_prefix_actually_spreads_across_partitions` — ⚠️ added after mutation testing
   showed `fnv -> 0` survived everything: once the names are in the path, uniqueness no longer
   needs the hash, so a constant prefix passes the collision test while putting **every roster
   in the fleet on one storage prefix**.
2. **A zone round-trips, and an absent one is refused.** `a_zone_survives_the_wire`,
   `a_frame_without_a_zone_is_refused` (`cargo test -p pstore-gossip --test wire`) — every
   truncation landing inside the trailing zone field decodes to `None`, never to an empty
   zone. A guessed zone is a node in the wrong cell.
3. ⚠️ **A cell's roster holds only its own members.** `a_cells_roster_holds_only_its_own_members`
   and `a_cell_with_no_members_is_empty_not_everyone` (`--test cell`). The second pins the
   dangerous fallback: an unknown zone yields **zero** members, never the whole view — "nobody
   here, so use everyone" is one global ring wearing a cell's name, and it fires exactly when
   a zone is new or has just lost its last node.
4. **A node without a zone refuses to start.** `a_node_without_a_zone_refuses_to_start`
   (`cargo test -p pstore-node --test policy`), against `policy::zone`, which is the decision
   without the environment. ⚠️ Split that way because mutating the process env is `unsafe` in
   this edition and `unsafe_code = "forbid"` is a workspace non-negotiable — a guard testable
   only by setting a variable could not be tested at all.
5. **No new blob requests.** `Roster::from_members` is a pure function of a view; the roster
   read is unchanged at 1 GET, still asserted by
   `reading_the_roster_costs_one_get_and_no_list` (`--test roster`).
6. **Placement never leaves the cell**, end to end. `placement_stays_inside_its_cell`
   (`cargo test -p pstore-cluster --test cell`): a 90-node view across three zones, a roster
   per zone, 1,000 keys each, every placed node in its own zone. ⚠️ The node's own `OWNS`
   reporting built its ring from the **unfiltered** view until this phase — a per-cell roster
   that one call site bypasses is the same bug in a smaller place.
7. ⚠️ **OQ-59 answered, and the answer contradicts what this milestone assumed.**
   `cargo run --release -p pstore-cluster --example az_balance` — imbalance (max node load ÷
   mean), 100,000 keys, R=3, 8 node-naming trials per row. **`provisional`: measured on WSL2.**

   | nodes | min | mean | max |
   |---|---|---|---|
   | 100 | 1.173 | **1.268** | 1.388 |
   | 300 | 1.240 | **1.333** | 1.435 |
   | 900 | 1.275 | **1.338** | 1.398 |

   **AZ-aware placement costs essentially nothing in balance.** At a fixed ring size the
   constraint is free — `place` has no AZ term, so a 300-node cell *is* a 300-node ring. And
   shrinking the ring does not hurt either: the trial spread (±0.1) is wider than the gap
   between 100 and 900 nodes.

   ⚠️ The mechanism runs **opposite** to the intuition the spec was written on.
   `window(n) = max(32, 3√n)` covers ~32% of a 100-node ring and ~10% of a 900-node one, and a
   relatively wider window balances better — it offsets the law of large numbers rather than
   compounding with it. The spec's wrong assumption is left visible, because it is why the
   first version of this criterion was unsatisfiable.
8. **A regression guard set from the measurement**: `imbalance_at_cell_scale_stays_bounded`
   holds a 300-node ring under **1.6×** across trials — above the observed 1.435 maximum, far
   below anything a clustering hash produces. ⚠️ Not 1.25×, which was M4a's bound at N=100.
10. ⚠️ **Detection is peer-relative, and the corpus's statistic had to be corrected.**
    `a_uniformly_slow_fleet_has_no_outlier` — multiplying every zone's latency by 10 changes no
    verdict — plus `a_zone_much_slower_than_its_peers_is_an_outlier` and
    `a_zone_failing_requests_is_an_outlier_even_when_fast`
    (`cargo test -p pstore-cluster --test gray`).

    ⚠️ **D-84's Envoy default cannot work at three zones**, and this is recorded as
    [C-9](../../research/04-cluster/gray-failure.md). With *n* samples the largest z-score any
    one can reach is `(n − 1)/√n`; at **n=3 the ceiling is 1.155**, so a factor of **1.9 never
    fires, whatever the degradation** — gray failure would have been undetectable at exactly
    the fleet shape D-79 prescribes. Excluding the candidate from its own baseline fixes the
    masking but not the sensitivity, because with two peers a standard deviation is nearly
    meaningless. The shipped test is a relative margin against the **peer median**.
11. ⚠️ **Load-correlated degradation sheds; infrastructure degradation drains (D-85).**
    `an_overloaded_zone_sheds_rather_than_draining` — the same latency at 95% utilization is
    `Shed` and at 25% is `Drain`. This is the criterion that stops the detector causing the
    cascade it exists to prevent.
12. **Draining requires headroom.** `draining_requires_headroom_in_the_survivors`: survivors at
    88–90% utilization turn a `Drain` into a `Shed`.
13. **A majority can never drain.** `a_majority_can_never_drain` — two of three degraded is a
    fleet-wide event, and the answer to one is not to switch the fleet off.
14. **Self-eviction is on others' evidence, except one case.**
    `a_node_evicts_on_others_evidence_and_not_its_own` (a `Shed` verdict must **not** evict, or
    shedding becomes draining and cascades) and
    `a_node_that_cannot_reach_the_store_evicts_itself` — D-87, the one case where
    self-assessment is reliable, and a node must not wait for a bulletin it cannot fetch.
15. **Coverage, mutation and gates.** `./scripts/coverage.sh --fail-under-regions 95` →
    **95.12%** region, 96.73% line. `cargo mutants -p pstore-cluster` → **27 caught, 0 missed
    = 100%** on `roster.rs` (from 70.4%) and **27 caught, 0 missed = 100%** on `gray.rs`.
    `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`,
    `cargo deny check`, `scripts/check-links.sh`, `scripts/build-index.py --check`,
    `scripts/check-verified.py` all green. A 40-node fleet converged end-to-end with cells
    wired in: all 40 nodes saw 40, placement settled at 81 of 1,000 shards against a fair
    share of 75.

 `cargo mutants -p pstore-cluster --file crates/pstore-cluster/src/roster.rs`
   → **27 caught, 0 missed = 100%**, from 70.4% before the two tests above. `cargo fmt
   --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`,
   `cargo deny check`, `scripts/check-links.sh`, `scripts/build-index.py --check`,
   `scripts/check-verified.py` all green. A 40-node fleet converged end-to-end with cells
   wired in (`GOSSIP=swim ./scripts/cluster.sh up 40`): all 40 nodes saw 40, placement settled
   at 81 of 1,000 shards against a fair share of 75.

## What review caught before any code was written

The first draft made `Cell` the roster **address** and stopped. Review found that incoherent:

⚠️ **A per-cell roster address is worthless while its contents come from the fleet-wide gossip
view.** A node in AZ-a would write AZ-b's members into AZ-a's roster and place across zones
anyway — with every test in the plan passing. And it could not be fixed as drafted, because
`pstore_gossip::Member` carried no zone: **a node cannot filter peers it cannot identify.**
That reordered the whole milestone; zone identity became phase 1 and placement became phase 2.

⚠️ **A pre-existing collision in M4a.** `roster.rs:36` addressed a roster as
`format!("{:04x}/clu/ROSTER", fnv(cluster) as u16)` — the cluster name **absent from the path
entirely**, so two clusters colliding in 65,536 shared one roster object and one ring.
`head.rs:55` had the right shape all along. Found by review, not by any test, and fixed here.

⚠️ **The first criterion 4 was unsatisfiable, and review proved it by measuring.** It required
each per-AZ ring to stay within the 1.25× balance bound "M4a set" — but M4a set that at
**N=100**, and the shipped placement measures 1.306 at N=300 and 1.374 at N=900. A 300-node
cell exceeds 1.25× in 23 of 24 samples. The only exits were to fail the milestone or to tune
the hash until the number appeared, which is the trap [M4a](../M4a/VERIFIED.md) records
already falling into once. OQ-59 moved to phase 2 with the measurement redesigned.

⚠️ **Two criteria could not fail.** "An AZ's placements are bit-identical after another AZ
changes" is a pure function compared against itself — every mutation perturbs both sides
equally. Restated over what actually needs pinning: how a cell's roster is *built*.

## Not run

⚠️ **This entry was stale and contradicted criterion 7 of this same ledger.** It described the
state *before* the measurement was redesigned — the first attempt compared one 900-node ring
against three 300-node rings, and review found the effect (0.026 on the mean) an order of
magnitude smaller than the spread across node-naming trials (0.28), so one sample per
configuration measured ring-position luck. That is all true, and it is why the harness was
rewritten to measure imbalance against `N` with eight trials.

**OQ-59 is answered by criterion 7.** The entry is kept rather than deleted because a ledger
that said both things at once is worth being able to find: the contradiction stood from M4e
until it was noticed while auditing what was still doable on this machine.

⚠️ **And criterion 7's load-bearing step is now a test.** Its argument is "`place` has no AZ
term, so a 300-node cell *is* a 300-node ring" — which holds only if a cell's placement depends
on nothing but its own roster. Probing that turned up a different gap and the test written for
it is `a_duplicate_node_id_is_not_placed_twice`: `from_nodes` sorts before deduplicating, and
`dedup` removes only **consecutive** duplicates, so without the sort a repeated id enters the
ring twice and takes double the load of its peers — silently, forever, and caught by nothing.
A roster is decoded from a blob other nodes wrote, so a repeated id is one gossip merge away.
