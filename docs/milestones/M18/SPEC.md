# M18 — A scheduled reap

**Serves:** [BACKLOG](../BACKLOG.md) row 31. The retention window of
[`compaction.md`](../../research/05-storage-engine/compaction.md) § GC is to keep dereferenced
objects for about an hour, then delete them.

## What is true today

- `Engine::gc(retention)` reaps graveyard entries buried at or below `HEAD.epoch - retention`.
  It is reached only by `POST /v1/admin/gc`, which an operator must call.
- Nothing calls it on its own, so:
  - HEAD's graveyard grows by an entry per dereferenced object, forever;
  - `reaped_before` stays 0, so `as_of` reaches back to the first epoch, and every open pays
    to read that history.
- The scheduled fold (M9i.1) is decided from memory, so an idle tenant costs no request. A
  timer that read every tenant's HEAD would cost requests per tenant per elapsed time, which
  AGENTS.md forbids.

## Delta

**The engine remembers what it made reapable, and when.**
- Each commit this engine makes records `(Instant, Epoch)`: a fold, a compaction, a drop or a
  branch.
- Every such commit buries something, since a fold buries the bundles it read.
- `Engine::reap_due(age)` returns the highest recorded epoch at least `age` old, or `None`.
  It reads only memory.

**A reap is bounded by time, not by an epoch count.**
- `Engine::gc_through(horizon)` is `gc`'s reap with the horizon given rather than derived.
- `gc(retention)` becomes `gc_through(HEAD.epoch - retention)` and is otherwise unchanged,
  as is the admin endpoint.
- Why the horizon is safe: epochs are totally ordered in time. Every HEAD at or below `horizon`
  was committed at least `age` ago, so a reader still holding one has been reading for longer
  than `age`.
- After a reap, the records at or below `horizon` are dropped. If the reap fails, they stay.

**The server reaps on the fold loop's pattern.**
- `GcPolicy { period, age }`, with a default `age` of 1 hour, as `compaction.md` specifies.
- `run_reaps` ticks every `period`:
  - It reaps each tenant whose engine reports a due horizon, and touches no other tenant.
  - Failures back off, as folds do, doubling from `period` up to `age`.
  - A stop signal halts new reaps and awaits those in flight.
- `serve_folding` starts it beside the fold loop, and one signal stops both.
- Configuration:
  - `PSTORE_GC=off` disables it.
  - `PSTORE_GC_AGE_S` and `PSTORE_GC_PERIOD_MS` set the policy. Anything that is not a
    positive integer is refused by name, as `PSTORE_FOLD_*` values are.

**Does not change:** what `gc` deletes, or when it refuses; the graveyard's format; the admin
endpoint; any request made by a write or a query.

## Acceptance criteria

1. **Idle costs nothing.** A tick over tenants that have committed nothing, or nothing older
   than `age`, issues no request.
2. **Due reaps.**
   - A tenant folds, compacts and drops an index. Time passes `age`, with paused tokio time.
   - One tick reaps. The buried segments, delete vectors and bundles are gone from the store.
   - HEAD has no graveyard entry at or below the horizon, and `reaped_before` is the horizon.
3. **Not before `age`.** The same tenant, one second short of `age`, is not reaped: its
   objects remain and no request is made.
4. **Nothing live is touched.** After the reap, every live query answers as before it.
   - `as_of` at the horizon or later answers as before.
   - `as_of` below the horizon is `time_travel_horizon`.
5. **Bounded by time, not by commit count.** A reap is due only for records `age` old. Records
   newer than that survive the reap, and their objects remain.
6. **Configured and stopped.**
   - `PSTORE_GC=off` runs no reap.
   - An invalid `PSTORE_GC_*` value is refused, naming the variable.
   - The reap loop has returned when `serve_folding` returns.
7. **A failed reap backs off, and keeps its records.** A store that refuses the reap's CAS: the
   next tick inside the backoff does not retry, and a later one reaps.
8. **Cost.** A due reap costs exactly what `gc` does: 1 HEAD read, one delete batch per
   1,000 keys, and 1 CAS.
9. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | no `reap_due` | a reap without a due record; a HEAD read to decide |
| 2 | nothing reaps | the loop not started; the horizon not passed |
| 3 | as 2 | `age` compared `>` for `>=`, or ignored |
| 4 | as 2 | a horizon past the due record |
| 5 | as 2 | records dropped past the horizon |
| 6 | the variables are ignored | the parse, the refusal, the stop |
| 7 | as 2 | no backoff; records dropped on failure |
| 8 | as 2 | an extra read |

## RA budget

A write and a query are unchanged. A reap is `gc`'s: 1 R, 1 W per 1,000 keys, and 1 CAS. It
runs at most once per tenant per `period` when due, and never for an idle tenant. So its
requests scale with the commits that buried objects, never with elapsed time.

## Risks

- **A query longer than `age`** can meet a reaped object and fail with a read error, never
  a wrong answer. Nothing enforces a query deadline yet. `compaction.md`'s window is sized
  as `query_timeout + snapshot_ttl + safety`.
- **A process that never commits never reaps**, even if others' objects are due. Whoever
  committed them reaps them.
- **Time travel** now reaches back about `age`, where it reached back to the beginning. That
  is the retention `mutations-and-mvcc.md` describes.

## Tasks

- **M18.1** — `reap_due` and `gc_through` in the engine, `GcPolicy` and `run_reaps` in the
  server, and the configuration.
