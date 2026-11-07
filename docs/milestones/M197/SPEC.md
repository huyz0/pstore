# M197: a reindex in place, under the index's own name

⚠️ **Closed, not built.** Spec review round 1 failed on two blockers and three majors, all of design:
- Rebuilt segments are keyed at the plan's epoch, so a query `as_of` an epoch between that one
  and the swap reads every row twice. The swap would have to re-seal at its commit epoch, as
  compaction does.
- A write that still declares the old options is refused after the swap, and unfolded rows
  that declared them are quarantined. So clients must switch their declared options at the
  swap, which is the same cutover a new name already needs.
- Without a schema history, a query `as_of` an epoch before the swap analyzes the old segments
  under the new options.
- The fingerprint leaves out the recorded types and further text fields.
- A cancel can lose to a sync that is already running.

**Closed, not built (owner's decision, 2026-11-07):** M168's reindex into a new name, plus a
client cutover, is G's answer. Nothing below is implemented.

## Serves
Workstream G of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Backfill
publishes a version atomically; a failed worker does not expose half an index", the "in-place
reindex under the same name" that [M168](../M168/SPEC.md) left pending.

## Delta
Today a reindex (M168) rebuilds a source into a **new** index. Moving clients to it is the
caller's job, and writes the source takes after the reindex's snapshot are not in it.

After:
- **Create.** `POST /v1/indexes/{index}/copy` with a `schema` and a source naming `index` itself
  (same tenant, no store) starts a reindex in place. Today that is refused ("an index cannot
  replicate itself"). The engine call is `Engine::reindex_in_place(index, Reindex)`. It is
  refused, as M168's options are, by `check_reindex`, and also:
  - with `as_of` or `read_only` (400);
  - for an index that does not exist, or has no folded segment (`source_not_found`);
  - for a replica, or the destination of a running copy (`409`);
  - while another in-place reindex of it runs (`409 replication_exists`).

  A repeated `POST` with the same options answers the running one (`200`).
- **HEAD** gains one trailing section after M196's `copy_as_of`: `rebuilds: BTreeMap<String,
  (u64 run, Reindex)>`, holding the in-place reindexes that are running. It encodes `Reindex` as
  M168's `reindexes` section does. A non-empty section forces every earlier count. A name is
  kept only while `indexes` holds it, so a drop ends the record with the index.
  `replication_gen` includes each record's name and run, so a server's register wakes the
  tenant's sync worker, as it does for a running replication.
- **The index stays an ordinary index while it runs.** Writes, folds, compactions, deletes and
  queries go on under the old options. It is not a replica, and nothing refuses a write.
- **Sync.** Each sync, after its replications, takes each record in its own HEAD:
  1. It rebuilds every folded segment of the index, as of the HEAD it read, under the
     options applied to that HEAD's schema. This uses M168's `rebuild`, with the index as its
     own source.
  2. It commits only if the commit's own HEAD still has the index's fingerprint (segments,
     delete vectors and schema) that it rebuilt from. Then, in one commit, the index's refs,
     delete vectors and schema become the rebuilt ones, and the old segments and vectors are
     buried at that epoch. The record ends `succeeded`, as a copy's outcome (M166) under the
     index's name, with `source_epoch` the epoch it rebuilt from and M194's times.
  3. If a fold, compaction, delete or schema change landed in between, nothing is published.
     What it wrote is buried, as a lost copy's is, and the next sync rebuilds again. The copy
     status's `last_error` says `the index changed during its rebuild`.
  4. A seal the format refuses ends it `failed` with `rebuild_refused`, as M168's does. The
     index keeps its old options.
- **Rows written but not folded** when the swap commits are folded later under the new
  schema, as any fold reads the schema HEAD holds. Rows folded during a rebuild force a retry.
  So an index that folds more often than one rebuild takes may never finish. **Setting it
  read-only (M149) stops its folds, and the next sync then finishes.** The docs say so.
- **Status and cancel:** `GET /v1/indexes/{index}/copy` reports a running in-place reindex as
  `running`, with `reindex` (M168's field) and the new `"in_place": true`, then its outcome as a
  copy's. `DELETE /v1/indexes/{index}/replication` cancels it: the record is removed, the
  outcome is `cancelled`, and the index keeps its old options. Pause and resume are refused
  (`409`).

Not delivered: progress beyond the state; a rebuild that reuses its earlier attempt's work;
type, dims, metric or id-kind changes (still re-writes, per M168's matrix).

Not changed: M168's reindex into a new name; copies; replications; a query's or a write's path.

## Acceptance criteria
1. **Swap** (engine): an index with text over 3 folds and a delete is reindexed in place with
   stemming. Before the sync, a stemmed query misses and the status is `running`. After one sync
   the status is `succeeded` with `source_epoch` the epoch it read. In one commit, the stemmed
   query hits, the row digest is unchanged, and the old segments are buried at that epoch. A
   write and fold after it is folded under the stemmed schema.
2. **A fold in between** (engine): a fold lands between the rebuild and the commit (driven by
   `replicate_with_interference_for_test`). Nothing is published, the old options still answer,
   what the attempt wrote is buried (no leak after `gc(0)`), and the status stays `running` with
   that `last_error`. The next sync, with the index read-only, finishes it. Every row written,
   including the fold's, is in the index.
3. **Refusals** (engine): each refusal in **Create** is checked, and HEAD's epoch is unchanged by
   each. A repeated create with the same options is matched.
4. **Ends** (engine): a refused seal ends it `rebuild_refused` with the old options answering. A
   cancel ends it `cancelled`, and a later sync publishes nothing. A drop of the index ends the
   record with it: HEAD as read back holds no record.
5. **HEAD** (unit): the section round-trips alone and after every earlier one. A HEAD without it
   encodes byte for byte as M196's. A cut never decodes it. A record for a name `indexes` lacks is
   not encoded. `replication_gen` changes with a record's run.
6. **Server:** `POST` with `schema` naming the index itself answers `202` with `"in_place":
   true`. After the worker runs, `GET` reports `succeeded`, and a stemmed text query hits. `POST`
   with `as_of` is `400`, and pause is `409`.
7. **Gates:** fmt, clippy, suite, coverage 95/95, mutants.

## Test plan
| AC | Test | Mutation it catches |
|---|---|---|
| 1 | engine `a_reindex_in_place_swaps_in_one_commit` | the swap leaving old refs; old segments not buried; schema not replaced |
| 2 | engine `a_fold_during_a_rebuild_publishes_nothing` | the fingerprint check dropped or read from the plan's HEAD; writes not buried |
| 3 | engine `a_reindex_in_place_is_refused_as_specified` | any refusal dropped |
| 4 | engine `an_in_place_reindex_ends_typed` | refused seal published; cancel not removing the record; record outliving a drop |
| 5 | `head.rs` unit `rebuilds_round_trip_after_copy_as_of` | section written when empty; counts not forced; filter dropped; gen ignoring records |
| 6 | server `a_reindex_in_place_over_http` | field routing, `in_place` not reported, refusals |

## RA budget
The create is one HEAD read of the tenant and one commit. A sync reads each source segment and
writes each rebuilt one, as M168's reindex does, plus no request: the commit is the sync's own
commit. An attempt lost to a fold costs one full rebuild, in bytes, and is retried each sync.
HEAD grows by one 4-byte count and one record per running in-place reindex.

## Risks
- A process older than M197 refuses a HEAD carrying the section (`CorruptHead`). Upgrade every
  node first.
- A busy index may not finish without being made read-only. The status's `last_error` says why.
- A writer older than M197 that folds while the record runs folds under the old schema. That is
  correct: its fold changes the fingerprint and forces a retry.

## Tasks
- M197.1: the engine call, HEAD section, sync step, status and cancel.
- M197.2: the server route, the docs, the support matrix row, and the DELIVERY row.
