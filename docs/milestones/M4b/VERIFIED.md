# M4b — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md), naming the test, command, or
measurement that demonstrated it.

Gate: `scripts/check-verified.py`.

⚠️ **Every wall-clock number here is `provisional`, and more so than the spec anticipated.**
The fleet runs on **host networking**, so the hundred nodes are a hundred *processes* sharing
one loopback, not a hundred network peers — see criterion 2's note. Convergence and traffic
are therefore a **floor**: a real network can only be slower. Criteria are stated in protocol
periods, which survive the transport; per-node RSS and CPU are unaffected, because the
container limits still apply.

1. **A joining node reads one object.** `reading_the_roster_costs_one_get_and_no_list`
   (`cargo test -p pstore-cluster --test roster`) for the count;
   `a_join_reads_the_roster_once_and_tolerates_an_empty_one`,
   `a_join_returns_the_members_already_present`
   (`cargo test -p pstore-node --test policy`) for the join path. One GET, zero LIST, no DNS,
   no service registry. Observed live: `JOIN node=… seeds=94` on a 100-node cold start.
2. **Convergence: 5–6 periods**, against a bound of ⌈log₃100⌉+3 = 8.
   `./scripts/cluster-report.py 200` on a cold 100-node fleet, all 100 nodes reaching a full
   view: **1.06s = 5 periods** and, on a second run under concurrent load, **1.19s = 6
   periods**. A range, not a single sample — a threshold pinned from one draw of a noisy
   statistic is the mistake [M4a](../M4a/VERIFIED.md) criterion 6 records. At 25 and 50 nodes:
   4 and 5 periods, so the growth is sub-linear as well as inside the bound. Measured from the **last** join, not the first: the harness
   staggers starts over ~40s and a node cannot list a peer that does not yet exist.
   ⚠️ The instrument was wrong before it was right. `VIEW` was printed once per second, which
   resolves to five periods — an instrument coarser than the eight-period bound it was
   checking, so its first answer ("9 periods") could not have been falsified. Nodes now emit
   `VIEWCHANGE` at the gossip period and `docker logs -t` timestamps it.
3. **Dead-node detection under 10% injected probe loss: 14 periods.**
   `./scripts/cluster.sh kill 5` on a fleet started with `./scripts/cluster.sh up 100 0.1`:
   `DETECTION, kill -> every survivor lists 95: 2.72s = 14 periods` (median 2.45s). Baseline
   without loss, same fleet size: **9 periods** (1.83s). The loss is real and counted —
   16,360 datagrams dropped across the run — and is injected in the node's own gossip
   transport (`Metered`), not with `tc netem`, which would have required `NET_ADMIN` on all
   100 containers to test an unprivileged fleet. Rate pinned by
   `an_injected_loss_rate_is_the_rate_that_is_injected`,
   `a_dropped_datagram_costs_no_bytes_and_is_not_an_error`
   (`cargo test -p pstore-node --lib`).
4. **OQ-12 — a degraded ACCUSER evicts nobody: PASS, with evidence that it was degraded.**
   `./scripts/cluster.sh starve pstore-n11 90` → `view 100 -> 100 of 100`, and
   `starvation evidence: accuser advanced 30 ticks, control 93`. The accuser ran at **32%**
   of a healthy node's loop rate — badly missing its deadlines — and still listed all 100
   peers. ⚠️ The harness now **fails** the test as `VACUOUS` if the accuser keeps pace with
   the control, because a `--cpus` quota that failed to apply would otherwise turn "a starved
   node evicted nobody" into a sentence about a node that was never starved.
   ⚠️ This answers OQ-12 for **phi-accrual**, not Lifeguard: `foca` (SWIM+Lifeguard) is
   MPL-2.0 and outside `deny.toml`'s allow-list, so the fleet runs `chitchat`. Same property,
   different mechanism — recorded rather than absorbed.
5. ⚠️ **Gossip cost is O(N) per node — this criterion FAILS, and the failure is the finding.**
   `./scripts/cluster.sh traffic 30` at three sizes: **22,776 / 37,423 / 75,391 bytes/s/node**
   at 25 / 50 / 100 nodes; a second 100-node run measured 77,459, so the top of the curve is
   reproducible to within 3%. Each doubling of the fleet multiplies the per-node cost by
   **1.64× then 2.01×** — against **2.0×** for O(N), **~1.2×** for O(log N), and 1.0× for
   O(1). It is linear, and not marginally so. This is the measurement that turns
   `membership.md`'s "zone-sharded gossip is required, not optional" from an assertion into a
   number, and M4b's Delta explicitly does not add it. Carried to **M4c**.
6. **Per-node cost at 100 nodes, reported and gated on nothing:** `./scripts/cluster.sh cost`
   → **mean RSS 3.25–3.28 MiB, mean CPU 0.50–0.84%** across two runs, so **50–84% of one
   core** carries the entire 100-node fleet and it fits in ~330 MB. At 25 and 50 nodes: 2.99
   and 2.87 MiB, so memory per node does not grow with the fleet.
   ⚠️ An earlier reading of "128% CPU per node" was the harness, not the fleet:
   `{{.MemUsage}}` expands to `3.4MiB / 128MiB`, and a space-split `awk` was summing the
   memory **limit** as a CPU percentage.
7. **Coverage, mutation and gates.** `./scripts/coverage.sh --fail-under-regions 95` →
   **95.29% region**, 96.08% function, 97.21% line on shipped crates.
   `cargo mutants -p pstore-node --file crates/pstore-node/src/policy.rs --file
   crates/pstore-node/src/transport.rs --timeout 120` → **45 caught, 1 missed, 0 timeouts =
   97.8%**, against a floor of 80%. The one survivor is `replace < with <= in
   Bernoulli::fires`, and it is **equivalent**: `u < p` and `u <= p` differ only when a
   53-bit uniform lands exactly on `p`, which no test can distinguish because nothing does.
   ⚠️ Three earlier runs scored lower and every point of the difference was a real hole in
   the tests, not a scoring artefact:
   * the backoff **schedule** was unasserted — `<<` could become `>>` and `base + jitter`
     could become `base - jitter`, with the suite green. The schedule is the only thing
     between a cold fleet and the herd that took it down, so `the_retry_schedule_actually_
     backs_off` now measures it on a paused clock, where virtual elapsed time *is* the
     schedule;
   * `jitter` never asserted its **defining** property — that a slot lies inside its period —
     so `%` could become `/` or `+`;
   * two mutants were recorded as `timeout` rather than caught, because a test awaited a
     datagram a dropping transport never sent, and because the gossip tests bound **fixed
     ports** while `cargo mutants` runs mutants concurrently. A test that hangs instead of
     failing converts a caught defect into an inconclusive one.
   `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`,
   `cargo deny check` (advisories, bans, licenses, sources ok), `scripts/check-links.sh`,
   `scripts/build-index.py --check`, `scripts/check-verified.py` all green.

## What this milestone got wrong

Six failures, five of which presented as a distributed-systems bug and were not.

⚠️ **The ARP cache, not the protocol.** Joins stalled at ~33 of 100 nodes with the blob store
idle at 1% CPU, every stuck node in `SYN_SENT`, and MinIO's listen queue showing zero
overflows. `dmesg`: `neighbour: arp_cache: neighbor table overflow!`. A bridged container
needs a host neighbour-table entry per node, and past `gc_thresh3` the kernel **drops packets
silently**. Raising the sysctl needs root on the WSL2 host, which a dev container does not
have; the fleet moved to **host networking**, which removes the entries rather than raising
the ceiling — and which is why every timing here is a floor.

⚠️ **A blob request with no timeout is worse than one that fails.** 64 of 100 joining nodes
sat inside a single GET for minutes — zero CPU, no log line, container `running`. The retry
budget never ran, because the attempt it was protecting had not returned. A node hung before
its first `println!` is indistinguishable from one that never started.

⚠️ **The roster could only ever hold one node's view.** Refold *replaces* the object, so a
node publishing only what it saw capped the roster at a single view — and a node absent from
it was dialled by nobody, because nobody held its address. The fleet settled into disjoint
groups: the roster stalled at 45 members while 20 nodes sat at `members=1` with a working
heal loop. It publishes the **union** now, and `union_to_publish` returns `None` when there is
nothing to add, so a converged fleet's steady-state write rate is zero.

⚠️ **`chitchat` seeds once.** A node whose seeds were all unreachable in that instant never
contacts anyone again, and no peer can reach it either. One node held a one-member view for
493 ticks while 96 peers converged without it. D-4 calls the blob store the partition-healing
backstop and it was not wired: the node now re-reads the roster every `HEAL_PERIOD` and dials
what it cannot see.

⚠️ **A node could not find itself.** `chitchat` advertises a *resolved* `SocketAddr`, so peers
saw `172.19.0.9:7946` while the config said `pstore-n9:7946`. Every node reported owning **0
of 1000 shards** while holding a 44-member view — which reads as a placement bug and is a
naming one. Pinned by `a_member_advertises_the_address_a_peer_would_dial`.

⚠️ **The node's decisions were untestable, so they were untested.** When to dial, what to
publish, how long to retry a join all lived inside `main`, measured at **0% coverage**, and
each of the four failures above was found by running a hundred containers rather than by a
test. They now live in `pstore_node::policy` with 14 tests; `main.rs` wires and nothing else,
and `scripts/coverage.sh` excludes binary entry points by a derived predicate — so logic left
in a binary is *invisible* to the gate, which is what makes the rule enforceable instead of
advisory.

## Deviations from the task list

**M4b.3 asked for `dev/cluster-compose.yml`; there is none, and `scripts/cluster.sh` is the
whole harness instead.** Compose scales a service by replication, and replicas are identical
— but on host networking every node needs its **own** advertise port, so a compose file would
have to enumerate 100 generated services. That is `cluster.sh` written twice, in a language
that cannot express the stagger, the loss parameter, or the teardown of the bridge. One
definition, in the file that already had to exist.

## Not run

**1,000 real nodes and 10,000 simulated**, which is M4's exit. 100 real nodes is what this
host can hold; the rest is `NOT-RUN`, not deferred effort. Zone-sharded gossip (criterion 5)
and the simulator are where the next order of magnitude has to come from.
