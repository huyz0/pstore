# M19 — Abandoned work buries what it wrote

**Serves:** [BACKLOG](../BACKLOG.md) row 30, and the leak M16's code review noted in `branch`
(a minor, recorded in that commit and not changed).

## What is true today

- A compaction seals its merged segment before it commits. A retry that moves the epoch
  seals again, at a new key.
- **When the commit lands**, every earlier retry key is buried under its own key epoch (M7e).
  GC reaps them, and `as_of` never resurrects them, because they were never live.
- **When the merge is abandoned instead**, nothing is buried, and every key it sealed is an
  object nothing names:
  - an input vanished or a delete vector changed under it, so it returns `Ok(None)`;
  - its last attempt lost, or the commit failed some other way.
- `gc` works only from the graveyard, so these objects stay, and are billed, until the orphan
  sweeper (M6e, specified but not scheduled) finds them.
- `branch` has the same shape. An attempt that lost its CAS wrote delete-vector copies, which
  the next attempt buries when it commits. When the retry is refused instead (`dest` now
  exists, or `src` no longer does), or fails, those copies are left unnamed.

## Delta

**Abandoned work is buried by one commit of its own.** When a compaction or a branch gives up
after writing objects, it reads HEAD fresh and commits one that differs only by those keys. Each
key is added to the graveyard **under its own key epoch**, exactly as M7e buries a retry's stale
keys, so `as_of` never resurrects it.
- **Every exit after writing buries, except a commit that landed.** That covers the discard
  (`Ok(None)`), the loop running out, a refusal on a retry, and any error, including a `?`
  after sealing or a copy PUT failing partway. The written keys are collected as they are
  written, and the burial runs in one place, around the whole body.
- **A key the HEAD being committed still names is never buried.** Compaction and branch keys
  are derived from the epoch, the lane and the index alone, so a winner on the same lane can
  have written, and committed, the very key the loser wrote. The burial drops every key named
  by that HEAD's `indexes` and every value of its `deletes`. This is checked in memory. GC's
  live check would keep the object anyway, but a live key in the graveyard is a contradiction
  `as_of`'s dedup would hide.
- The burial CAS is retried on `Lost` against a fresh HEAD, up to `MAX_COMMIT_ATTEMPTS`. It is
  recorded for the scheduled reap (M18).
- It never changes what the abandoning call returns. If it cannot land, the objects stay
  unnamed, as today.
- A merge that sealed nothing, and a branch that copied nothing, commit no burial.

**Does not change:** the success paths, their costs, or what they bury; `as_of`; the graveyard's
format; the orphan sweeper's job, which stays whatever a crash leaves.

## Acceptance criteria

The other writer below is on a **different lane**, unless the criterion says otherwise.

1. **A discarded compaction is buried.** Another compaction of the same index commits first,
   through `compact_with_interference_for_test`, and ours returns `Ok(None)`.
   - Every segment key ours sealed is in the graveyard, under its own key epoch.
   - After `gc(0)`, none of its objects remain in the store, sidecars included.
   - The winner's segment and every live row are untouched.
2. **A compaction whose delete vector moved** (a fold deletes a row of an input meanwhile)
   is discarded and buried the same way.
3. **A refused branch retry is buried.** A branch's first attempt loses its CAS after copying
   a delete vector, and `dest` is created meanwhile, so the retry is refused. The copies are
   in the graveyard, `gc(0)` removes them, and `dest`'s rows are untouched.
4. **The past is unchanged.** For each scenario, `as_of` answers exactly as it did before the
   burial commit, at every buried key's own key epoch and at every epoch up to the burial.
5. **A same-lane winner's key is not buried.** Two compactions on one lane seal the same key,
   and the winner commits it. The loser's burial leaves that key out, and `gc(0)` keeps it.
6. **Cost.** An abandoning call costs what it did before, plus 1 HEAD read and 1 CAS, plus
   1 read and 1 CAS per `Lost` retry of that CAS. A merge that sealed nothing, and a success,
   add nothing.
7. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | the merged key is not buried | the discard not burying; `out_key` left out of the burial |
| 2 | as 1 | the second discard path not burying |
| 3 | the copies are not buried | the refusal not burying; `copies` left out |
| 4 | a past epoch answers with the discarded merge | burying at the committing epoch in place of the key epoch |
| 5 | the winner's key is in the graveyard | the live-key filter removed |
| 6 | an extra request on a success | a burial on a path that wrote nothing, or on success |

## RA budget

The success paths are unchanged. An abandoned compaction or branch adds 1 R and 1 CAS, plus
1 R and 1 CAS per `Lost` retry. That happens only when optimistic work lost, never per row,
and never on a query or a write.

## Risks

- **A burial that cannot land** leaves the objects unnamed, as today. A backend that fails the
  merge's commit usually fails the burial too.
- **A crash** between sealing and burying still leaks. That is the orphan sweeper's job, and
  it is still unscheduled.

## Tasks

- **M19.1** — the burial commit, and its use in `compact_inner` and `branch_inner`.
