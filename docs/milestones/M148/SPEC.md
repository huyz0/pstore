# M148: an index reports its bytes and when it was created and last written

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
  Unix milliseconds. It follows `keys_since` and forces every optional section before it,
  by the rule each section since M15.2 follows. With `keys_since` absent it is written as
  a marker byte 0 and with it present as 1 followed by the value, so `times` can follow
  either. Absent means no index has a time (a HEAD from before M148).
- **Who sets it.** `now` is the committing process's clock, made monotonic per index:
  `written_ms = max(previous written_ms, now)`, and `created_ms` is never moved.
  - `create_index` (M147) sets both to `now`.
  - A fold sets `written_ms` for every index it commits an operation for (a row, a
    delete, a patch, a by-filter operation, rejects or quarantine counted for it), and
    `created_ms` too when the index had no entry.
  - A branch sets both for its destination; a replica sync that commits sets `written_ms`
    for its replica, and both on the sync that creates it.
  - Compaction and GC set nothing: they change no row. So `written_ms` is not
    `updated_epoch`, which a compaction moves.
  - Dropping an index removes its entry, as it removes its schema.
- **The summary** adds `created_at` and `updated_at` (RFC 3339 UTC, millisecond
  precision, `null` for an index with no entry) and `approx_logical_bytes`: the sum of
  its segments' recorded lengths (M45), `null` when any of its segments has no recorded
  length. Deleted rows still count until a compaction, which is why it is approximate.
  Nothing is read for it beyond the one HEAD the summary already reads.

Not changed: query, write and list behaviour; `updated_epoch`; the request counts of every
route; HEAD's other sections. Mixed-version clusters: a node from M132 to M147 refuses a
HEAD with this section as corrupt (its decoder requires the end after `keys_since`), so
every node of a deployment runs M148 before any of them commits; as M129 states for its
section, that is the operator's step, and rollback after a commit is not supported.

## Acceptance criteria
1. A created index reports `created_at == updated_at`, both within the test's clock
   window around the create, and `approx_logical_bytes` 0.
2. A write and a fold move `updated_at` forward and leave `created_at`; a compaction
   moves neither; a fold of another index moves neither.
3. An index that came into being by a write gets both on its first fold. A branch's
   destination gets its own `created_at`; a drop removes the entry, and a re-creation
   gets a new `created_at`.
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
  `first_fold_creates_the_time` (3), `branch_and_drop_times` (3) kill each setter removed
  or misplaced. A clock is injected (`Engine::with_clock_for_test`) so (5) can run a
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
- Clock skew between nodes makes `created_at`/`updated_at` approximate wall time; the
  monotonic rule keeps them ordered per index, not correct across nodes.

## Tasks
- M148.1: the HEAD section, its encoding and the setters, with the injected clock.
- M148.2: the summary fields and tests; docs (api-migrations, DELIVERY).
