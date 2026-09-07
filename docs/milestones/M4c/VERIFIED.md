# M4c — Verified (Phase A)

One line per acceptance criterion in [SPEC.md](SPEC.md), naming the test, command, or
measurement that demonstrated it.

Gate: `scripts/check-verified.py`.

⚠️ **Three criteria FAIL, and two of them were mis-specified by me rather than missed by the
code.** They are recorded as failures with the numbers, not adjusted to fit. Which criteria,
and why, is the most useful thing in this ledger.

⚠️ Wall-clock numbers are `provisional` and name the period they were taken at. The fleet
runs on host networking — a hundred processes on one loopback, not a hundred network peers —
so timings are a floor. `dev/README.md` has the reason.

1. **Wire format round-trips and refuses garbage.** `every_message_round_trips`,
   `a_corrupt_frame_is_an_error_not_a_member`, `an_unknown_state_tag_is_refused`
   (`cargo test -p pstore-gossip --test wire`). Truncation at every length is refused, and so
   are trailing bytes — a decoder that ignores a desync carries it forward silently.
2. **A suspicion is refutable.** `a_wrongly_suspected_node_refutes_and_returns`,
   `a_dead_node_stays_dead_against_a_stale_message`, `a_node_never_suspects_itself`
   (`cargo test -p pstore-gossip --test cluster`). A refutation at an equal incarnation is
   rejected — that is what a replayed packet looks like.
3. **Convergence at 100 nodes: 6 periods**, against a bound of 8.
   `./scripts/cluster-report.py 200` on a cold fleet, all 100 reaching a full view.
4. ⚠️ **Steady-state cost is O(1) — the ratio passes, the absolute misses by 3%.**
   `./scripts/cluster.sh traffic 30`, 200ms, real fleets: **764 B/s/node at 100** and
   **1,057 at 1,000** — a **1.38×** spread for ten times the nodes, against the required
   <2× and against `chitchat`'s measured **6.2×** over the same range. But the criterion also
   said <1 KB/s at both, and 1,057 is over 1,024. **It fails, by 33 bytes.**
   At the shipping 1s period both are far inside it: 153–251 B/s at 100, 242–594 at 1,000.
   The protocol's own cost, measured exactly in-process by
   `cargo run -p pstore-gossip --example cost`, is **74 bytes per node per round, identical at
   100, 1,000 and 10,000 nodes** — two messages, a ping and an ack. `chitchat`, measured on a
   real fleet, was ~13,000 bytes per round at 100 nodes.
5. ⚠️ **Fleet CPU at 1,000 nodes: 1.90 cores against a bound of 1. FAILS, and the bound was
   written against a floor I had not measured.** `./scripts/cluster.sh cpu 60` at a 1s period.
   Decomposed, by measurement rather than argument:
   * **0.72 cores** is the floor for a thousand near-idle containers — measured by running the
     same fleet at a 10s period, where the protocol does almost nothing;
   * **0.73 cores** is the protocol;
   * **0.47 cores** is the `OWNS` diagnostic, which is not the protocol at all.

   So "≤1 core" left 0.28 cores for everything above a floor of 0.72, and no protocol could
   have met it. The comparison that means something: `chitchat` cost **~17 cores** on the same
   fleet, at load average 1162; this costs 1.90 at load 11.6.
6. ⚠️ **Detection under 10% injected loss: 31 periods against a bound of 14. FAILS, and the
   bound was copied from a protocol with different properties.** Measured with
   `./scripts/cluster.sh up 100 0.1` then killing five and timing every survivor's
   `VIEWCHANGE`: **6.29s = 31 periods** (median 5.84s).

   The 14 came from M4b's `chitchat` measurement, and buying it back would mean shortening the
   suspicion window — which is exactly what made healthy nodes die. **Measured at 100 nodes
   under 10% loss with a constant window: the fleet flapped indefinitely**, 88 nodes at 100
   members, 11 at 99, one at 98, with nothing actually wrong. With the window scaled as
   3·log₂N the same fleet holds **100 of 100, stable**. Slower detection is what a stable view
   under loss costs, and the honest restatement is that a detection bound has to scale with N
   the way the window does.
7. ⚠️ **OQ-12, re-asked for this detector and passed.**
   `./scripts/cluster.sh starve pstore-n11 90` → `view 100 -> 100 of 100`, with
   `starvation evidence: accuser advanced 53 ticks, control 94` — the accuser ran at **56%** of
   a healthy node's loop rate and evicted nobody. The harness fails the test as `VACUOUS` if
   the accuser keeps pace, so a quota that failed to apply cannot be mistaken for a pass.
8. **A partition heals.** `a_healed_partition_converges_to_one_member_set`
   (`cargo test -p pstore-gossip --test protocol`). ⚠️ **Simulation only.** Splitting a real
   fleet needs network control this environment does not have on host networking; that is
   `NOT-RUN`, and the simulation exercises the real protocol rather than a model of it.
9. **Coverage, mutation and gates.** `./scripts/coverage.sh --fail-under-regions 95` →
   **95.70%** region, 97.43% line. `cargo mutants -p pstore-gossip --timeout 60` →
   **87 caught, 15 missed = 85.3%**, against a floor of 80%; most survivors are in the
   peer-selection hash, where any peer is a valid choice. `cargo fmt --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`, `cargo deny check`,
   `scripts/check-links.sh`, `scripts/build-index.py --check`, `scripts/check-verified.py`
   all green.

## Phase B is not built, and criterion 4 is why

The gate said hierarchy would be built only if Phase A failed to make per-node cost
independent of the fleet. It did not fail: **1.38× across a tenfold increase**, and 74 bytes
per round flat to 10,000 nodes in-process. Groups and delegates would be a tier added against
a problem that no longer exists.

⚠️ What survives, and is **not** answered here: cross-AZ traffic is billed at $0.02/GB round
trip ([`az-topology.md`](../../research/04-cluster/az-topology.md)), and that is an argument
about which peers a node probes rather than about tiers of nodes. It belongs with per-AZ
cells in **M4e**.

## What this milestone got wrong

⚠️ **The simulation was more generous than the network, and certified a bug.** It delivered
replies within the same round, so an ack was instantaneous, a probe timeout could never
elapse, and a 10%-loss test passed against a protocol that flapped on a real fleet. Messages
now arrive the period after they are sent. The lesson is not "simulate better" — it is that a
simulation that cannot fail is worth exactly what a test that cannot fail is worth.

⚠️ **The same test passed at twelve nodes and failed at a hundred.** An every-Nth drop pattern
missed the interleavings that matter. Fleet size was a test parameter chosen for speed, and it
was hiding the defect.

⚠️ **Three cost hypotheses were wrong before one was right**, and all four were measured:
the `OWNS` diagnostic (free at 100 nodes, 25% at 1,000), the member-list clone (free at both),
`piggyback` never draining (**the actual bug** — every probe carried the same six records
forever, 6,000 bytes/s/node against a predicted 370), and the per-container floor.

⚠️ **Two harness bugs that hid results rather than causing them**: the survivor count was
re-read from `docker ps` immediately after `docker kill`, so it counted a container on its way
out and then waited for a number no node would report; and the kill timestamp used nine digits
of nanoseconds, which `datetime.fromisoformat` refuses, so the detection report died silently
and printed nothing at all.
