# M192 — A replica's lag is bounded: the worker folds the source, reports lag, and syncs on request

**Serves:** D-39 (a fold is triggered by size or age, and its rate is the cost; this adds one
opt-in trigger); OQ-135 (async replication), as [M22](../M22/SPEC.md) built it; the
"Bounded replication lag" proposal in the project notes of 2026-11-03.

## What is true today
- A replica shows its source's **folded** state. The source folds at 1 MiB or 1 hour of
  unfolded bundles (`PSTORE_FOLD_BYTES`, `PSTORE_FOLD_AGE_S`), decided from the memory of the
  process that flushed them. A trickle of writes takes up to ~1 hour to reach a replica.
- A fold is tenant-scoped and discovered, not remembered (`Engine::fold`): **any** process
  can fold any tenant. Nothing on the replica's side uses that.
- Nothing measures lag. `GET …/replication` gives `last_commit_ms`, which does not move while
  the source is idle, so it says nothing about staleness.

## Delta

**Bound.** `PSTORE_REPLICATION_MAX_LAG_S = L` (unset: no bound, M22 unchanged; at least 2).
For each held tenant, every replication whose source is in **this server's own store** (not
a copy, M166) is *bounded*. Every `L/2` the worker folds each distinct bounded source tenant
through this process's engine for it (`Engine::fold`), then syncs at once. The tenant is due
at the earlier of its sync interval and its fold time, so `idle` never stretches past `L/2`.
- A replication's **fresh** time is the start of the last source fold that succeeded and was
  followed by a sync that committed it or found it current. A write acknowledged before that
  instant is on the replica. **Lag** = now − fresh; before any, now − when this worker first
  saw the replication.
- **Exceeded** when lag > `L`, judged at each sync of the tenant. A failed source fold is that
  replication's `last_error` (`source fold: …`); fresh does not move.
- A source in a **remote store** is not bounded: its credentials are read-only, so the worker
  cannot fold it. Its status says so (`max_lag_ms: null`).

**Notes.** `REPLSTATUS`'s note gains `fresh_ms`, `max_lag_ms` and `lag_exceeded`, all
optional, so either version reads the other's. Written as before (changed, and at most once
per `ttl/3`), except a change of `lag_exceeded`, or a replication's first `fresh_ms`, is
written at once.

**Status.** `GET …/replication` adds `max_lag_ms`, `fresh_ms`, `lag_ms` (this server's clock
minus `fresh_ms`) and `lag_exceeded`, from the note. ⚠️ `fresh_ms` may be up to `ttl/3` older
than the worker's own; `lag_exceeded` is the worker's judgment and is never late by more than
one sync.

**Metrics** (no tenant dimension): `pstore_replication_source_fold_total{outcome}` (folded,
nothing, failed), `pstore_replication_lag_exceeded_total` (each change into exceeded) and the
gauge `pstore_replication_lagging` (bounded replications this worker holds that are exceeded).

**Sync now.** `POST /v1/indexes/{dest}/replication/sync`, `write` scope, with the source's
authority re-checked as a create's is. It folds the source when it is in this store, then
syncs the tenant's replications, and answers the status plus `source_folded` and, when folded
and `dest` committed or was current, `fresh_ms` = the fold's start: every write the source
acknowledged before the call is then on the replica. `404 replication_not_found`;
`409 replication_paused`; `503` when the fold or `dest`'s sync failed.

**One sync per engine.** `Engine::replicate` takes an engine-wide gate, so the worker's sync
and a sync-now on one process never run together: M22 left that to the worker alone, and two
syncs on one engine derive the same copy keys.

**Unchanged:** HEAD's format, the register, M22's sync, refusals and costs when `L` is unset;
folds of the source by its own writers. No LIST, no lock across processes: a fold is a CAS.

## Acceptance criteria

1. **Bounded.** With `L` = 10 s, one write to the source and no fold by its writer: the
   replica answers it within `L` of paused time, by a worker in another process. Without `L`,
   it does not within 10 `L`.
2. **Idle stays bounded.** After 5 minutes of an idle source, a write still arrives within `L`
   (the interval did not double past `L/2`).
3. **Status.** While bounded: `max_lag_ms` = `L`, `fresh_ms` set, `0 <= lag_ms <= L`,
   `lag_exceeded` false. A remote source's: `max_lag_ms` and `fresh_ms` null, false.
4. **Exceeded.** With the source's bundles unreadable, after `L`: `lag_exceeded` true,
   `last_error` names the source fold, `lag_exceeded_total` = 1 and `lagging` = 1, and the
   failed fold counted. Readable again: false within one fold, and `lagging` = 0.
5. **Sync now.** A write, then `POST …/sync` with no worker running: `200`, `source_folded`
   true, `fresh_ms` set, and the replica answers the write. Paused: `409`. Unknown: `404`.
   A principal without the source's authority: `403`, and nothing folded.
6. **One sync per engine.** A second `replicate` on an engine waits while the first is held
   mid-sync, and both end with the replica equal to its source.
7. **Config.** `PSTORE_REPLICATION_MAX_LAG_S` unset is none; `1`, `0` and `x` are refused by
   name.
8. **Gates.** `./scripts/gates.sh` green; coverage floors hold.

## Test plan

| # | Test that fails first | Mutation it catches |
|---|---|---|
| 1 | `server` `replication_lag::a_write_reaches_the_replica_within_the_bound`, `…_without_a_bound_it_waits_for_the_fold` | no fold; fold of the dest tenant instead of the source; sync not run after the fold |
| 2 | `replication_lag::an_idle_source_stays_bounded` | `due` ignoring the fold time; cadence `L` doubled |
| 3 | `replication_lag::status_reports_the_lag`, `a_remote_source_is_not_bounded` | fresh taken at the sync's end; `lag_ms` sign; remote folded |
| 4 | `replication_lag::a_failing_source_fold_is_exceeded_and_recovers` | exceeded never set or never cleared; `>` as `>=` around `L`; counter per tick |
| 5 | `replication_lag::sync_now_*`, `auth_sources::a_sync_now_needs_the_sources_authority` | no fold; `fresh_ms` without commit or current; paused not refused; authority unchecked |
| 6 | `engine` `replicate::a_second_sync_waits_for_the_first` | gate dropped |
| 7 | `replication_lag::replication_max_lag_is_validated` | bound below 2 accepted |

## RA budget

- **Unset:** unchanged.
- **Bounded, per distinct same-store source tenant, per `L/2`:** one fold, billed to the
  **source** tenant (it is that tenant's engine): with nothing unfolded, 1 HEAD GET, the
  lane registry and one tail probe per live lane; with bundles, D-39's fold. Then the sync's
  M22 cost (1 source GET when unchanged). Depth: fold, then sync, in the background.
- ⚠️ **Per elapsed time on purpose**, as M22's polling is, and opt-in by the operator. At
  `L` = 10 s and 2 lanes, ~4 GETs per 5 s = ~70,000 GETs a day per bounded source.
- **Notes:** one PUT per held tenant per `ttl/3` while bounded (fresh moves every cycle).
- **Sync now:** one fold and one sync, as above, depth 2 + M22's.

## Risks

- **Billing across tenants.** A replica's worker folds another tenant, billed to it; its
  owner granted the reader the source (M127), and a fold changes no answer. Revealed by the
  source tenant's spend on an idle index.
- **More folds, smaller segments.** `L` small makes a trickle fold often; compaction pays.
- **Status coarseness.** `fresh_ms` is a note, so `lag_ms` can read up to `ttl/3` high.
- **Every server must set the same `L`**, as with sources; a worker reports the `L` it uses.

## Tasks

- **M192.1** — engine: the one-sync gate (test 6).
- **M192.2** — server: the bound, notes, status, metrics, sync now, config, docs (tests 1–5, 7).
