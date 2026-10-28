# M160: an index reports its indexing progress across every writer

Spec review round 1 failed with two majors. The probe used `lanes::tail`, whose doubling
window can take dozens of rounds and thousands of reads on a stalled fold, and costs 8
reads on every idle lane; it is replaced by one probe per lane and one budgeted forward
round. And a new index durable only in another process's WAL was 404, which left the
durable-but-unindexed state unanswerable; with `progress=true` it now answers. Round 2
passed with one minor, folded in: the freshness guarantee holds only when `exact` is true.

## Serves
Workstream G of [the delivery plan](../../comparisons/turbopuffer-next-plan.md):
"Metadata distinguishes durable/unindexed/indexed states and folded row counts ...
Progress is not derived solely from the serving process's buffer. Freshness is explicit;
metadata gathering uses bounded reads." The comparison's metadata row lists
`index.status`, `unindexed_bytes` and `unindexed_rows` as missing, with only a
process-local `unfolded` boolean ([comparison](../../comparisons/turbopuffer-2026-10-25.md)).

## Delta
Today `GET /v1/indexes/{index}` reports the folded state from one HEAD read (segments,
documents, `epoch`, M148's times and bytes) and `unfolded`: whether **this process**
holds rows for the index that no fold has committed. A write acknowledged by another
process, durable in its lane's WAL and not yet folded, is invisible to it. The engine
already finds such writes for a `strong` read (M9i.2): it reads the lane registry beside
HEAD and probes each lane's next bundle at HEAD's watermark, without a LIST.

After:
- **`GET /v1/indexes/{index}?progress=true`** adds a `progress` object (not `index`, which
  the summary already uses for the index's name):
  `{"status": "up-to-date" | "updating", "unindexed_rows": n, "unindexed_bytes": n,
  "exact": bool, "as_of_epoch": e}`. Without the parameter the summary is unchanged,
  `progress` is absent, and its cost is the one HEAD read it is today. Any other value of
  `progress` than `true` or `false` is `400`.
- **What it counts.** Every durable bundle of every registered lane past HEAD's
  watermark for that lane, as of the HEAD read: for each, the bundle's own directory
  (its footer) gives the index's operation count and encoded byte length.
  `unindexed_rows` sums the operation counts (upserts, patches, deletes and by-filter
  operations alike: operations not yet folded), and `unindexed_bytes` the byte lengths.
  `status` is `updating` when `unindexed_rows > 0`, else `up-to-date`. Rows this or any
  process holds only in memory, not yet flushed to a bundle, are not durable and are not
  counted; `unfolded` keeps reporting this process's.
- **How it reads, bounded.** A constant depth of at most five rounds, and at most
  `lanes + PROGRESS_BUNDLES` probes and `2 × PROGRESS_BUNDLES` range reads, with
  `PROGRESS_BUNDLES = 64`:
  1. HEAD and the lane registry together (two reads), as a `strong` read does.
  2. One `head` per registered lane, at its watermark, all at once, as `settled` does
     for a `strong` read. An absent bundle means the lane has nothing unfolded. `head`
     returns the bundle's length on every backend.
  3. For the `p` lanes found present, one round probing each lane's next
     `max(0, PROGRESS_BUNDLES / p - 1)` sequence numbers, all at once, stopping each
     lane's count at its first absent key (a lane is dense). A lane whose probed keys
     are all present may have more: `exact` is false.
  4. Each found bundle's directory, at most `PROGRESS_BUNDLES` of them, by one absolute
     range read of its last `min(len, 4 KiB)` bytes, all at once. The fixed 28-byte
     footer is last and the directory sits right before it, so the footer gives the
     directory's offset and length against the bundle's length.
  5. A directory longer than that read, with one more absolute range read of exactly
     its bytes.
  When more than `PROGRESS_BUNDLES` lanes are present, the lowest-numbered 64 are read
  and `exact` is false. No LIST, no suffix read (so Azure, which has none, answers too),
  no request per record, and no doubling probe: depth does not grow with how far a
  fold is behind.
- **Freshness is explicit.** `as_of_epoch` is the HEAD epoch the counts are relative to.
  A fold committing during the request can make the counts include operations that
  folded epoch is already serving. When `exact` is true they never omit an operation that
  was durable before the HEAD read and is not in that HEAD; when it is false, operations
  past the budget are uncounted, and a name whose only operations lie there is 404.
- **Engine.** `Engine::progress(index) -> Result<Progress, EngineError>` with
  `Progress { epoch, rows, bytes, exact }`. It reads only; it never folds or flushes.
- **An index durable but not yet folded.** Without `progress`, as today: 404 unless HEAD
  holds it or this process holds rows for it unfolded. With `progress=true`, a name
  that neither holds but whose name has an entry in a bundle directory read answers
  200, as an empty index (0 segments, 0 documents, no schema, null times) with
  `progress.status` `updating`. A directory does not say whether its operations are rows
  or deletes, so a name whose unfolded operations are all deletes answers so too, and
  is 404 again after the fold, which (M9f.2) makes no index of deletes alone. Otherwise
  404.

Not changed: the summary without `progress`; every other route's request count; HEAD's
encoding; the WAL and bundle formats.

## Acceptance criteria
1. Without `progress`, the summary is byte-for-byte the same JSON and costs one read.
2. With `progress=true` after a durable write in another process (a second `Api` over
   the same store, a different lane) and before any fold, for an index HEAD holds and
   for one it does not yet: `status` is `updating`, `unindexed_rows` is the operations
   written to that index (upserts plus deletes), `unindexed_bytes` equals the sum of
   that index's entry lengths in the bundles, and `exact` is true. Rows of another
   index in the same bundles are not counted.
3. After a fold of everything: `up-to-date`, 0 and 0, and `as_of_epoch` is the fold's
   epoch.
4. With more than `PROGRESS_BUNDLES` unfolded bundles in one lane: `exact` false, and the
   counts equal the operations in the 64 lowest bundles. With two present lanes, each is
   probed 31 keys past its watermark.
5. A directory longer than the first range read is read with one more range read, and
   counted exactly.
6. Cost: the request's reads are HEAD, the registry, at most `lanes + 64` probes, and
   one range read per bundle read (two for a long directory), with no LIST and no
   write. An idle tenant with three registered lanes costs HEAD, the registry and
   three probes.
7. `progress=maybe` is 400; an absent index is 404 with or without `progress`.
8. Gates: clippy, the workspace suite, coverage 95/95, mutation of the changed code.

## Test plan
- Engine unit (`bundle`): `a_directory_reads_from_the_tail` (3, 5) kills an offset
  computed from the buffer rather than the bundle's length, and a long directory
  decoded from a short read.
- Engine: `progress_counts_other_lanes` (2, 3), `progress_is_bounded` (4: 65 bundles,
  each with one row, and two present lanes), `progress_costs` (6, by counting store
  requests by kind).
- Server: `summary_progress` (1, 2, 3, 7).

## RA budget
The summary without `progress`: unchanged. With it: at most five rounds whatever the
backlog (HEAD and registry; a probe per lane; one budgeted forward probe; directory
reads; a long directory). At most `lanes + 64` probes and 128 range reads. Requests
scale with registered lanes (as a `strong` read's do) and are capped in bundles; never
with records.

## Risks
- A tenant pays one probe per registered lane per request, idle lanes included, and the
  registry only grows. It is opt-in, and a `strong` read pays the same.
- The counts are operations, not resulting rows: a delete of an absent id counts, and an
  upsert folded with a later delete of the same id counts twice. The field's doc says so.

## Tasks
- M160.1: the tail directory reader in `bundle`, and `Engine::progress` with its probe.
- M160.2: the summary parameter and field; docs (api-migrations, DELIVERY, comparison).
