# M148: an index reports its bytes and when it was created and last written

Spec review round 1 failed with two majors: an index that existed before M148 would get a
false `created_at` at its first fold, and times could be kept for names that do not exist
(delete-only folds, a discarded quarantine), growing HEAD and reviving an old
`created_at`. Both are fixed below by the existence rule and one invariant, with the
minors. Round 2 passed with two minors, folded in: the keys are a subset, and the
pruning is in `Head::encode`.

## Serves
Workstream G of [the delivery plan](../../comparisons/turbopuffer-next-plan.md):
"Metadata distinguishes ... folded row counts, with consistent timestamps and size
definitions ... metadata gathering uses bounded reads", and the comparison's metadata row
(`created_at`, `updated_at`, `approx_logical_bytes`; [comparison](../../comparisons/turbopuffer-2026-10-25.md)).

## Delta
Today `GET /v1/indexes/{index}` reports segments, documents, epochs and reject counts. It
has no wall-clock time and no size: HEAD records neither, and epochs are not times.

After:
- **HEAD gains one trailing section**, `times`: per index, `(created_ms, written_ms)`,
  Unix milliseconds, `created_ms` 0 meaning unknown. It follows `keys_since` and forces
  every optional section before it, by the rule each section since M15.2 follows. When
  `times` is non-empty, `keys_since` is written as a marker byte (0 absent, 1 present)
  followed by its value when present, then the `times` count and entries. An empty
  `times` is never written, so a HEAD without it encodes byte for byte as M147's. The
  decoder tells the forms apart after the outcomes: exactly 8 bytes left is M147's raw
  `keys_since`; anything else is the marker form, which with one entry is at least 21
  bytes. Absent means no index has a time (a HEAD from before M148).
- **The invariant**: the keys of `times` are a subset of the indexes that exist in the
  HEAD being written (an index made before M148 and not written since has none), existence as drop and create decide it (a key of `indexes`,
  `schema_rejects` or `quarantine`). It is enforced in one place, `Head::encode`, which
  every HEAD write goes through and which skips the entry of any name that does not
  exist. So a delete-only fold of an absent
  name, a drop, and a discarded last quarantine all leave no entry, and a name created
  again gets a new `created_at`.
- **Who sets it.** `now` is the committing process's clock, read inside each commit
  attempt and made monotonic per index against the HEAD that attempt read:
  `written_ms = max(previous written_ms, now)`; `created_ms` is never moved. An index
  that did not exist in the HEAD the attempt read gets `created_ms = now`; one that
  existed with no entry (made before M148) gets `created_ms = 0`, reported as `null`.
  - `create_index` (M147) and a branch's destination: both times.
  - A fold: `written_ms` for each index in its `touched` set and the keys of its reject
    counts, after duplicate idempotency keys are discarded, the dropped index removed,
    and a replica's rows set aside. So a retried duplicate request moves nothing.
  - A replica sync: `written_ms` for each replica in the commit's `included` set (both
    times on the sync that creates it). A source's compaction is synced, so it moves the
    replica's `updated_at`; that is the one exception to compaction moving nothing.
  - Nothing else sets a time: compaction, GC and its reap record, `bury_abandoned`,
    the external index routes (an external is not an index here: its summary is 404),
    replication create, pause, resume and cancel (cancel keeps the index and its times),
    and quarantine discard (which may end an index, and then the invariant drops it).
    So `written_ms` is not `updated_epoch`, which a compaction moves.
  - `updated_at` is when a write was folded, not when it was acknowledged: it lags an
    acknowledged write by up to a fold interval.
- **The summary** adds `created_at` and `updated_at` (RFC 3339 UTC, millisecond
  precision, `null` for an index with no entry) and `approx_logical_bytes`: the sum of
  its segments' recorded lengths (M45), `null` when any of its segments has no recorded
  length. That is the encoded segment objects: rows, index structures and dictionaries,
  not delete vectors or the quarantine; deleted rows count until a compaction, and a
  branch's shared segments count in each index that names them. The definition goes in
  api-migrations.
  Nothing is read for it beyond the one HEAD the summary already reads.

Not changed: query, write and list behaviour; `updated_epoch`; the request counts of every
route; HEAD's other sections. Mixed-version clusters: a node from M132 to M147 refuses a
HEAD with this section as corrupt (its decoder requires the end after `keys_since`), so
every node of a deployment runs M148 before any of them commits; as M129 states for its
section, that is the operator's step, and rollback after a commit is not supported. A node
from M129 to M131 does not check for the end: it drops `keys_since` and `times` when it
re-commits.

## Acceptance criteria
1. A created index reports `created_at == updated_at`, both within the test's clock
   window around the create, and `approx_logical_bytes` 0.
2. A write and a fold move `updated_at` forward and leave `created_at`; a compaction
   moves neither; a fold of another index moves neither.
3. An index that came into being by a write gets both on its first fold; one that
   existed before M148 (a HEAD without `times`) gets `updated_at` and a `null`
   `created_at`. A branch's destination gets its own `created_at`; a drop removes the
   entry, and a re-creation gets a new `created_at`. A delete-only write to an absent
   name leaves no entry; discarding the quarantine of an index that lived only by it
   removes its entry. A duplicate idempotency key's retry moves nothing.
4. `approx_logical_bytes` equals the sum of the summary's segments' object lengths read
   back from the store, and is `null` for a segment without a recorded length.
5. A process whose clock is behind the last writer's does not move `updated_at` back.
6. HEAD round-trips with `times` beside each combination of absent and present
   `keys_since`; a HEAD without `times` encodes byte for byte as M147's.
7. The summary's request count is unchanged (1 HEAD read).
8. Gates: clippy, the workspace suite, coverage 95/95, mutation of the changed code.

## Test plan
- Engine: `times_round_trip_beside_keys_since` (6) kills a marker that confuses absent and
  present `keys_since`, or `times` not forcing the sections before it.
- Engine: `fold_sets_written_time_only_for_its_indexes` (2), `compaction_sets_no_time` (2),
  `first_fold_creates_the_time` (3), `an_index_from_before_m148_has_no_created_time` (3),
  `branch_and_drop_times` (3), `times_never_outlive_their_index` (3: delete-only fold,
  discarded quarantine, re-creation) and `a_duplicate_key_moves_no_time` (3) kill each
  setter removed or misplaced, the invariant not enforced, and existence read from the
  times entry rather than the HEAD. A clock is injected (`Engine::with_clock_for_test`) so (5) can run a
  process behind the last writer and kill a non-monotonic `max`.
- Server: `summary_reports_times_and_bytes` (1, 4, 7) kills a byte sum that skips a
  segment or reports 0 for an unknown length.
- A replica test (3) kills a sync that sets no time.

## RA budget
Unchanged for every route. HEAD grows by one name and 16 bytes per index, bounded by the
index count HEAD already carries; no new object, no request per index or per record.

## Risks
- A setter missed on some commit path leaves `updated_at` stale. Revealed by the per-path
  tests; the fold's set is the batch's index keys, so a new operation kind is covered.
- The invariant in `encode` is what keeps HEAD bounded. A caller's in-memory `next` may
  still hold a skipped entry; nothing reads `times` from its own `next` (the summary
  reads HEAD again).
- Clock skew between nodes makes `created_at`/`updated_at` approximate wall time; the
  monotonic rule keeps them ordered per index, not correct across nodes.

## Tasks
- M148.1: the HEAD section, its encoding and the setters, with the injected clock.
- M148.2: the summary fields and tests; docs (api-migrations, DELIVERY).
