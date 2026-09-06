# M4b — Membership and a 100-node fleet

**Serves:** D-4's gossip half (SWIM + Lifeguard), and **OQ-12** — "does Lifeguard's
false-positive suppression hold when nodes are CPU-pinned by SIMD work", which is this
milestone's reason for existing and which the first draft of M4a failed to cite.

**Depends on:** [M4a](../M4a/SPEC.md) — placement is what a member is *for*.

⚠️ **Scope, stated before anything is built.** M4's exit asks for 1,000 real nodes and
10,000 simulated. This does **100 real nodes in Docker**: measured, an idle container costs
~1.4 MB of host memory, so 100 is ~140 MB against ~20 GB free, and the binding cost is the
node process. 1,000 real nodes needs hardware this environment does not have; that is
`NOT-RUN`, not deferred effort.

⚠️ **Every wall-clock number here is `provisional`.** 100 processes on one host bridge see
microsecond latency and near-zero loss, and the dev container is capped at 6 CPUs — so
convergence measured in seconds is measuring Docker and the scheduler. **Criteria are stated
in protocol periods**, which survive the transport; wall-clock is reported beside them and
gated on nothing.

## Delta

**Adds**
- Gossip membership. ⚠️ **Adopted, not written**: the roadmap says "SWIM+Lifeguard via
  `chitchat`/`foca`", and a from-scratch SWIM is a multi-week subsystem sitting among
  single-commit tasks. Criterion 2 is therefore honestly an *integration* test of a
  library's property, and the ledger will say so.
- `pstore-node` — joins, gossips, and reports what it would own. **Not a query server**: a
  server would make every measurement here about something else. Layer 4.
  `node_id` is a **uuid generated at process start**, never derived from IP or hostname, so a
  restart looks cold (`membership.md`).
- `dev/cluster-compose.yml` and `scripts/cluster.sh` — bring up *N*, measure, tear down.

**Does not add** — caching and the cache dip (M4c); per-AZ cells and gray failure (M4d);
zone-sharded gossip, which `membership.md` calls *required, not optional*, and whose absence
is why criterion 4 measures growth rather than an absolute.

## Acceptance criteria

1. **A joining node reads one object.** One GET, zero LIST, no DNS, no service discovery.
2. **Convergence in protocol periods**: from a cold 100-node start, every node lists all 100
   within **⌈log₃ 100⌉ + 3 = 8 periods**. Wall-clock reported, `provisional`, gated on nothing.
3. **A dead node is detected** by every survivor within a stated number of periods, **with
   10% probe loss injected** — the parameter is in the criterion, not in prose, because an
   uninjected run measures Docker.
4. ⚠️ **OQ-12: a degraded ACCUSER evicts nobody.** A node whose own probe loop is CPU-starved
   must not evict healthy peers. The first draft tested the opposite — pausing a *target*,
   which is indistinguishable from death, so a correct detector *should* evict it and the
   test could only pass against a broken one.
5. **Gossip cost grows sub-linearly**: bytes/s/node measured at **25, 50 and 100** nodes fits
   O(1) or O(log N), not O(N). One measurement is not a slope.
6. **RSS and CPU per node reported** at 100 nodes — `reported, not gated`, stated so the
   ledger records it as a measurement rather than a passed criterion.
7. Coverage, mutation and gates as elsewhere.

## Risks

- **Docker is not a datacentre**, which is why criteria 2–4 carry injected loss and are
  counted in periods.
- **Adopting a gossip crate means criterion 4 tests a library**, not our algorithm. Worth it:
  writing SWIM to test SWIM is how a milestone becomes a quarter.
- **100 nodes is one order below M4's exit.** What holds here may not at 10,000; the
  simulator is where that goes.

## Tasks

| ID | Task |
|---|---|
| M4b.1 | Adopt a SWIM+Lifeguard crate; join from the M4a roster |
| M4b.2 | `pstore-node`: uuid identity, join, gossip, report placements |
| M4b.3 | `dev/cluster-compose.yml`, resource-capped per node |
| M4b.4 | `scripts/cluster.sh`: N nodes, convergence and cost, tear down |
| M4b.5 | Fault injection: probe loss, and a CPU-starved accuser (OQ-12) |
