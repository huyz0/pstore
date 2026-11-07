# M200: a running copy, export or reindex reports its progress

## Serves
Workstreams G and H of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): G's
"backfill progress" and H's "progress and other operation kinds" remainders in
[DELIVERY](../../comparisons/DELIVERY.md). A reindex (M168) is G's backfill, and it runs as a
copy, so one status field answers both rows' progress. Other operation kinds stay pending.

## What holds today
- A copy, an export (M167) and a reindex (M168) are each one record in HEAD's `replications`,
  with `once` naming it. A sync copies or rebuilds every segment it needs in one pass
  (`mirror`, `rebuild`), then publishes everything in one commit.
- `GET /v1/indexes/{dest}/copy` reports `running` and then the outcome, with `created_at` and
  `ended_at` (M194). Nothing says how far a running copy has got. The work happens on one node
  and is not written to HEAD until it publishes, so no other node can say either.

## Delta
- **Checkpoints.** While a sync copies or rebuilds for a record in `once`, after each batch of
  `checkpoint` segments it commits the record's progress to the destination tenant's HEAD. A
  batch is a segment copied (`mirror`) or a segment rebuilt (`rebuild`). `checkpoint` is an
  engine setting, default 64, set by `Engine::with_copy_checkpoint(n)`. The last batch is not
  checkpointed: the publish follows it.
  - A checkpoint is one HEAD read and one conditional write. It is written only while HEAD's
    record still has the run the sync planned. It leaves every other field of HEAD as read.
  - It is best-effort. A conditional write that loses is retried after a fresh read, at most 3
    times in all, and then that checkpoint is skipped. A skipped or failed checkpoint never
    fails, delays or changes the copy. The next checkpoint, or the publish, supersedes it.
- **Progress counts one sync's attempt.** `segments_total` is the segments the attempt must
  copy or rebuild, `segments_done` the ones it has. `bytes_done` is the segment bytes they hold
  (a rebuild counts the bytes it wrote). When a remap finds the source changed, the new
  segments are added to `segments_total`. A sync that starts again (another node, or after a
  crash) starts again from 0, because the earlier attempt's objects are not reused (they carry
  its epoch). The status says which attempt by its `updated_at`.
- **HEAD** gains one trailing section after M198's: `copy_progress: BTreeMap<String,
  Progress>`, where `Progress { run: u64, segments_done: u32, segments_total: u32, bytes_done:
  u64, updated_ms: u64 }`. Varints throughout. It is kept only for a name `once` holds whose run
  matches, so a cancel, a drop or an outcome ends it. The publish, the failure and the cancel
  each remove it in their own commit. It forces the earlier counts, as every section does.
  Malformed input (a duplicate name, `segments_done > segments_total`, a cut) is `CorruptHead`.
- **Status.** `CopyStatus` gains `progress: Option<Progress>`. The route's running copy answers
  `"progress": {"segments_done", "segments_total", "bytes_done", "updated_at"}`, or `null`
  before the first checkpoint (and for a copy of `checkpoint` segments or fewer, which goes
  straight to its outcome). An outcome answers `null`.
- **Replications** (not `once`) are not checkpointed. They keep running, and their own status
  is unchanged.
- **Docs:** `docs/api-migrations.md` says what `progress` counts, that it counts one attempt,
  and that it is updated every `checkpoint` segments, not continuously.
- **Deploy readers first:** a reader before M200 refuses a HEAD holding the section
  (`CorruptHead`), as for M196 and M198.

## Acceptance criteria
1. **A copy reports its way through** (engine): an index of 10 folded segments copied with
   `checkpoint` 3. A test hook runs after each checkpoint commit and reads the copy's status
   from a second engine over the same store. It sees `running` with progress `(3, 10)`,
   `(6, 10)` and `(9, 10)`, in that order, each with `bytes_done` larger than the last and the
   run of the copy. After the sync the status is `succeeded`, `progress` is `None`, and HEAD as
   read back holds no `copy_progress` entry.
2. **A reindex reports the same** (engine): AC 1 for a reindex with stemming. The counts are
   segments rebuilt.
3. **Best-effort** (engine): a write to another index lands before every checkpoint's
   conditional write (the hook writes and flushes and folds). The copy still succeeds, with the
   rows a copy without the writes holds, and every checkpoint it reports is in order. A store
   that refuses every conditional write of HEAD for a while (3 in a row) skips that checkpoint
   and the copy still publishes.
4. **Ends** (engine): a cancel between two checkpoints removes the entry in the cancel's commit,
   and the sync's next checkpoint writes nothing (its run is gone). A copy that ends
   `epoch_unavailable` removes it in that commit.
5. **HEAD** (unit): the section round-trips alone and after every other section. A HEAD without
   it encodes byte for byte as before. A name `once` lacks, or a run that differs, is not
   encoded. Each malformed input above is refused. No cut decodes the section.
6. **Cost:** a copy of `checkpoint` segments or fewer issues the requests it issues today (the
   existing replication cost tests stay green unchanged). A copy of `n` segments that writes
   `k = ceil(n / checkpoint) - 1 > 0` checkpoints, none lost, adds exactly `2k + 2` requests:
   each checkpoint's GET and conditional PUT, and one lost publish (the sync's publish was
   planned on the HEAD before its checkpoints, so its first conditional PUT loses and it reads
   HEAD again, as for any write that lands during a sync). The engine test counts them.
7. **Server:** `GET /v1/indexes/{dest}/copy` answers `"progress": null` for a running copy
   before its first sync, and `null` for a succeeded one. The engine field is mapped to the
   route by a unit test of `describe_copy`.
8. **Gates:** fmt, clippy, suite, coverage 95/95, mutants.

## Test plan
| AC | Test | Mutation it catches |
|---|---|---|
| 1 | engine `a_copy_reports_its_progress_at_each_checkpoint` | no checkpoint; the last batch checkpointed; counts off by one; bytes not added; the entry kept after the publish |
| 2 | engine `a_reindex_reports_segments_rebuilt` | `rebuild` not checkpointed |
| 3 | engine `a_lost_checkpoint_never_fails_a_copy` | a lost checkpoint failing the sync; no retry |
| 4 | engine `a_cancel_ends_a_copys_progress` | the run not checked; the cancel not removing it |
| 5 | `head.rs` unit `copy_progress_round_trips_after_ann` | section written when empty; filter dropped; counts not forced; a malformed entry accepted |
| 6 | in AC 1's test, the request count | a checkpoint written for a copy that fits one batch |
| 7 | server `a_running_copy_reports_progress` and unit `progress_is_described` | field not mapped |

## RA budget
A checkpoint is 1 HEAD GET and 1 conditional PUT, at most 3 of each when it loses, once per
`checkpoint` segments copied, and a copy that writes any adds one lost publish (a GET and a
conditional PUT): requests scale with the segments a copy moves, so with its bytes,
never with records, indexes or time. Nothing is added to a write, a query, a fold, or a copy of
`checkpoint` segments or fewer. Each running copy adds about 12 bytes plus its name to HEAD
until it ends.

## Risks
- A checkpoint is a HEAD commit, so it contends with the tenant's writers. Its cap of 3 tries
  and its skip keep a busy tenant's copy unchanged, at the cost of a stale `progress`.
- A sync that dies leaves its last checkpoint in HEAD until the next sync's first checkpoint or
  outcome. `updated_at` shows how old it is.
- `segments_total` can grow while a remap runs, so a client must not read `done / total` as
  monotone across a remap.

## Out of scope
Progress for a replication, for a fold or a compaction, and an ETA. Reusing an earlier
attempt's copied objects.
