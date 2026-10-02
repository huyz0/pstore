# M26 — The engine registry is bounded

**Serves:** [BACKLOG](../BACKLOG.md) row 25, which [M7c](../M7c/VERIFIED.md) opened: one `Engine`
per tenant is held for the life of the process, which is 1M of them at the scale the README
claims.

## What is true today

- `pstore-server`'s `Api::engine` builds an engine on a tenant's first request, and keeps it until
  the process exits.
- An engine holds state no HEAD read can rebuild:
  - **`pending`:** rows acknowledged `batched`, visible to this process and not yet flushed;
  - **`durable`:** rows flushed and not yet folded, which this process's reads fuse in;
  - **an uncertain bundle write** (M17), resolved before its next write;
  - **`abandoned`:** names its fold must still bury (M23);
  - **`reapable`:** commits a scheduled reap is waiting to collect (M18).
- The scheduled fold (M9i) and reap (M18) find their tenants by walking this registry.
- Everything else an engine holds is rebuilt from HEAD, or reset as a restart resets it:
  - schemas, the HEAD cache and the replica names, from HEAD;
  - its lane position, by M9j's resume, which probes forward from the watermark and is what M17
    calls safe on a restart;
  - its last commit (`committed`), which restarts at 0 until its next commit, as on a restart. A
    write or GC response reports that epoch.
- A fold, a replication commit and a GC each leave a reapable entry, cleared only once the reap
  age (1 h by default) has passed. So a tenant that commits is busy for that long after.

## Delta

**Beyond a cap, the server drops the least recently used engines that are idle: those whose
eviction a restart of that tenant alone would make, losing nothing.**

- **Idle**, a new `Engine::is_idle(count_reapable: bool)`. It is answered from memory, with no
  request, and holds when:
  - `pending` and `durable` are empty;
  - there is no uncertain write and no abandoned name, and, when `count_reapable`, no reapable
    commit;
  - the lane is not known to be taken (M17's sticky refusal).
- **And the server's own marks** (spec review minor 1): a tenant a refused `strong` read asked a
  fold of (M9i.2), or whose fold or reap is backing off, is busy too.
- **In use** is never evicted. The registry holds each engine in an `Arc`, and one whose
  `strong_count` is above 1 has a request in flight. Dropping it would let the next request build
  a second engine on the same lane in one process.
- **The cap:** `PSTORE_ENGINES`, default 10 000, `0` for unbounded (today's behaviour).
  - **Enforced in a loop of its own, never on a request** (spec review M2, round 2).
    - `serve` runs it once a second whenever `PSTORE_ENGINES` is non-zero, whatever
      `PSTORE_FOLD`, `PSTORE_GC` and `PSTORE_REPLICATION` say.
    - A reapable commit counts as busy only while a reap loop runs. With `PSTORE_GC=off` nothing
      would ever collect it, so keeping the engine would bound nothing.
  - When the registry holds more than the cap, it evicts idle, not-in-use engines, least recently
    used first, until it holds `cap - cap / 10`, in integers.
  - A scan that frees nothing, and found no engine in use, backs off, doubling from 1 s up to
    60 s. The backoff resets when a scan frees something. A scan is not counted as fruitless if
    it found an engine in use that would otherwise have been idle, since another tick's
    snapshot may be what held it. So a server over its cap with nothing idle scans at most once
    a minute.
  - Nothing ever refuses or delays a request for the cap. Between ticks, and while nothing is
    idle, the registry may hold more than the cap.
- **Last use** is recorded on every `Api::engine` call: an `Instant` beside the engine. It is the
  only addition to the request path.
- **Hooks:**
  - `Api::limit_engines(cap)` sets the cap, and `main` calls it from `PSTORE_ENGINES`.
  - `Api::evict_tick()` runs one eviction pass. It is `#[doc(hidden)]`, as the other ticks are,
    for tests.
  - `Api::engines()` reports the registry's size.
- **Metrics:** `GET /metrics` reports `pstore_engines` (the registry's size) and
  `pstore_engines_evicted_total`.

**Not changed:**
- the engine, apart from `is_idle(count_reapable)`;
- durability, visibility and consistency for any write;
- the scheduled fold and reap;
- M9j's resume and M17's lane check, which an evicted tenant's next engine goes through exactly
  as a restarted process does.

## Acceptance criteria

Each criterion makes its tenants idle the way production does: written, flushed, folded, and
reaped with `reap_due` at age 0 (spec review M1).

1. **The registry is bounded.** With a cap of 4 and 20 idle tenants, one eviction pass leaves 4
   engines (`cap - cap / 10` = 4). Evictions are counted.
2. **Unflushed rows are never evicted.** A tenant with a `batched` write not yet flushed survives
   any number of passes, and its rows stay visible to this process's queries.
3. **Nor is any other unrebuildable state.** A tenant with any of the following is never evicted:
   - flushed rows not yet folded;
   - an uncertain bundle write;
   - an abandoned name;
   - a reapable commit;
   - a taken lane;
   - a requested fold;
   - a fold or reap backing off.
4. **Nor an engine in use.** An engine whose `Arc` a caller holds is not evicted.
5. **Least recently used first.** Of two idle tenants, the one used longer ago is evicted.
6. **An evicted tenant comes back whole.** A tenant is evicted, asserted by the registry's size
   and the count. Its next requests then read every row it had, write and flush without
   `lane_taken`, and fold.
7. **The cap is soft, and its scan backs off.** Over the cap with nothing idle:
   - a new tenant's request is served;
   - a pass frees nothing;
   - the next pass within the backoff does not scan, which is counted.
8. **Independent of the other duties.** A server with fold and GC off still evicts, and with GC
   off a tenant that committed is evictable.
9. **No request is spent on it.** A pass makes no blob request (a regression guard: `is_idle`
   is synchronous).
10. **Gates.** `./scripts/gates.sh` is green, and the mutation sweep over M26's source diff misses
   0, every miss closed by a test or named as equivalent.

## Test plan

Engine tests of `is_idle` go in `crates/pstore-engine/tests/idle.rs`, one per kind of state.
Server tests go in `crates/pstore-server/tests/registry.rs`, with `limit_engines`,
`evict_tick` and `engines`.

| # | Test | Must fail first because / mutation it catches |
|---|---|---|
| 1 | server `the_registry_is_bounded` | today: no eviction |
| 2 | server `unflushed_rows_are_never_evicted` | `pending` left out of `is_idle` |
| 3 | engine `each_kind_of_state_keeps_an_engine_busy`, server `a_requested_or_backing_off_tenant_is_kept` | any one term dropped |
| 4 | server `an_engine_in_use_is_not_evicted` | the `strong_count` check dropped |
| 5 | server `the_least_recently_used_goes_first` | most recent first; last use not recorded |
| 6 | server `an_evicted_tenant_comes_back_whole` | a lane resumed wrongly; rows lost |
| 7 | server `the_cap_is_soft_and_a_fruitless_scan_backs_off` | refusing past the cap; no backoff |
| 8 | server `eviction_runs_with_fold_and_gc_off` | eviction riding another loop; reapable busy with GC off |
| 9 | server `eviction_costs_no_request` | regression guard |

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Any request, an eviction pass | unchanged | unchanged | unchanged | 0 | unchanged |
| An evicted tenant's first read | 0 | 1 (HEAD) | 0 | 0 | 1, as a restarted process |
| Its first flush | as today | 1 (HEAD) | 9 (8 lane probes and the lanes registry) | 0 | +2, as a restarted process |

## Risks

- **Thrash.** A working set larger than the cap rebuilds engines continually: a HEAD read and 9
  parallel reads on each tenant's first flush after an eviction. `pstore_engines_evicted_total` shows it, and raising
  `PSTORE_ENGINES` answers it.
- **The cap bounds only the quiet.** A tenant that commits is busy until its fold is due (up to
  1 h or 1 MiB) and its reap age has passed (1 h), so the cap bounds tenants that have not
  committed in roughly the last 2 h. A server whose committing tenants alone exceed the cap stays
  over it, and the backoff keeps that cheap.
- **A tenant that never folds** stays busy, and is never evicted. The scheduled fold bounds it,
  since it folds what this server wrote. With `PSTORE_FOLD=off`, such a tenant stays busy until
  an outside fold and its next HEAD read.
- **A stranded `durable` batch.** A flush/fold race can leave a batch below the pruned watermark,
  which only the tenant's next HEAD read clears. Such a tenant stays busy until its next request.

## Tasks

- **M26.1** — `Engine::is_idle` and its test.
- **M26.2** — The registry's cap, eviction in its own loop with its backoff, last use, the server's
  marks, `PSTORE_ENGINES`, metrics, the hooks, the `Api` doc that calls the registry unbounded,
  `deploy.md`, and the server tests.
- **M26.3** — The ledger, `BACKLOG.md` row 25 closed, and the roadmap row.
