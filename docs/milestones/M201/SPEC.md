# M201: GC announces its horizon before it deletes

## Serves
Workstream H of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "A concurrent
writer/GC cannot make an acknowledged export silently inconsistent". [M196](../M196/SPEC.md)
left one window open (its Risks, **The GC window**), and the H row in
[DELIVERY](../../comparisons/DELIVERY.md) names it: "a hold that keeps GC off an epoch being
copied ... pending".

## What holds today
- `gc_inner` reads HEAD, works out the horizon, deletes every unreferenced object buried at or
  below it (with its sidecars), and only then commits `record_reap(horizon)` and prunes those
  graveyard entries, all in one commit.
- An `as_of E` copy (M196) re-reads the source's view at `E` in every commit attempt that
  would publish it. A view below `reaped_before` ends the copy `epoch_unavailable`.
- **The window.** Between GC's first delete and its commit, `reaped_before` still says `E` is
  answerable. A copy whose GETs and re-read both fall inside that window can read a segment
  whose sidecars are already deleted, take them as never written (as M166 does for an absent
  sidecar), pass its re-read, and publish. Nothing reports it.

## Delta
- **GC announces before it deletes.** A pass whose horizon is above HEAD's `reaped_before`
  first commits `record_reap(horizon)`, through the engine's commit (a new epoch and nonce).
  The graveyard is kept in that commit. Only after it lands does the pass delete. It then
  prunes the graveyard entries it deleted in a second commit, planned on the HEAD the first
  commit wrote (`commit_head_at`, M200), so it reads no HEAD unless the commit loses.
  - A lost or contended announcement is retried from a fresh read, as today. Nothing has
    been deleted at that point.
  - A lost prune is retried from a fresh read. It removes the graveyard entries at or below
    that HEAD's `reaped_before`, and it deletes nothing again.
  - Deletes stay before the prune, so a failed delete still loses no record. The comment in
    `gc_inner` explains why.
- **The announced entries are reaped first.** A pass reaps the graveyard entries at or below
  `max(horizon, reaped_before)`. Entries at or below `reaped_before` are already announced, so
  deleting them is safe. A pass that died between its announcement and its prune (a crash, or
  a failed delete) leaves them for the next pass, whatever retention that pass is given. When
  every entry due is already announced, the pass skips the announcement and goes straight to
  the deletes and the prune.
- **What it closes.** Every GC delete now follows a HEAD in which the epochs it affects are
  below `reaped_before`. A copy's re-read comes after its GETs, so either the re-read sees the
  announcement and ends the copy `epoch_unavailable`, or every GET came before the first
  delete. The same holds for a copy in another tenant or store, whose re-read reads the
  source's HEAD (M196).
- **Not a hold.** GC does not wait for a copy. A copy whose epoch GC passes still ends
  `epoch_unavailable`, and the client asks again at a newer epoch. A query `as_of` an epoch
  GC is reaping is unchanged: retention is what protects it.
- **Docs:** `docs/api-migrations.md` says that an `as_of` copy or export either holds its
  epoch whole or ends `epoch_unavailable`, and that GC costs one more conditional write per
  pass that reaps.

Not changed: what GC reaps, `gc`'s and `gc_through`'s return values, `reap_due`, HEAD's
encoding (no new section), copies, queries.

## Acceptance criteria
1. **Announced first** (engine): a store hook runs at a GC pass's first `delete_batch` and
   reads HEAD from a second engine over the same store. Its `reaped_before` is already the
   pass's horizon. After the pass the graveyard holds no entry at or below it, and `gc`
   returns the count it returns today.
2. **The window closes** (engine, for a same-tenant, a cross-tenant and a remote source): an
   `as_of E` copy is created, and a GC whose horizon is past `E` runs. At its first delete, a
   hook runs the copy's whole sync. The copy ends `failed` with `epoch_unavailable`, `dest`
   holds no segment, and the sync commits no copy. Against the parent the same test publishes
   the copy, so the test is seen red.
3. **A pass that dies is finished by the next** (engine): a store refuses a GC pass's first
   `delete_batch`, and the pass returns that error. HEAD's `reaped_before` is raised and its
   graveyard kept. A second pass, given a retention whose horizon is below `reaped_before`,
   deletes those objects, prunes those entries, and returns their count. A third pass returns
   0 and writes nothing.
4. **Cost:** a pass with nothing due issues the requests it does today (one HEAD GET). A pass
   that reaps, none lost, issues 1 GET, its delete batches and 2 conditional PUTs (one more
   than today). `a_scheduled_reap_costs_what_gc_does` changes from `(1, 1, 1)` to `(1, 1, 2)`,
   which is this spec's cost and needs the owner's OK. Every other GC test stays green
   unchanged.
5. **Gates:** fmt, clippy, suite, coverage 95/95, mutants.

## Test plan
| AC | Test | Mutation it catches |
|---|---|---|
| 1 | engine `gc_announces_its_horizon_before_it_deletes` | deletes before the announcement; the prune dropped |
| 2 | engine `a_copy_inside_a_gc_pass_never_publishes` | the announcement dropped, or committed after the deletes |
| 3 | engine `an_announced_reap_is_finished_by_the_next_pass` | the due range not raised to `reaped_before`; the announcement not skipped when nothing new is due |
| 4 | engine `a_scheduled_reap_costs_what_gc_does` (amended), `gc_with_nothing_due_is_free` (unchanged) | a third PUT; the prune re-reading HEAD |

## RA budget
One more conditional PUT of HEAD per GC pass that reaps something, and a GET only when a
commit loses. GC passes scale with a tenant's scheduled reaps, never with records, indexes or
elapsed time per index. Nothing changes on a write, a query, a fold or a copy.

## Risks
- A process older than M201 still deletes before it commits. A mixed fleet keeps the window
  open until every node that runs GC is upgraded. HEAD's format does not change, so mixing is
  otherwise safe.
- Between the announcement and the prune, `as_of` refuses epochs whose objects still exist.
  That is the conservative direction.

## Out of scope
A hold that keeps GC off an epoch a copy reads. Measured RPO/RTO. Progress for other
operation kinds.
