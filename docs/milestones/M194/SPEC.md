# M194: a copy reports when it was created and when it ended

Spec review round 1 failed on two majors. The first: a replication "clearing" the record was an
equivalent mutation, because the encode filter already drops it. The second: ending a copy
created before M194, which has no record, was undefined. Both are addressed below, with the
minors. Round 2 failed on one major: a mutation row claimed what a fixed clock cannot
see. That claim is now dropped, and the capture rule is checked by code review. Round 3 passed. Amended in code review: AC 3's earlier copy is a failed one, because a
cancelled copy leaves an index whose name a new copy is refused; AC 5 bounds the times by the
wall clock, since the server has no clock seam.

## Serves
Workstream H of [the delivery plan](../../comparisons/turbopuffer-next-plan.md), row
"Restartable asynchronous operation status/results": "wall-clock times, progress and other
operation kinds pending" ([DELIVERY.md](../../comparisons/DELIVERY.md)). This milestone delivers
the wall-clock times. A caller can now tell how long an operation has run, and when it ended,
without polling and keeping its own clock.

## Delta
Today a copy's status (M166, and M167's export and M168's reindex) gives a `run` token and the
`epoch` that ended it, but no wall-clock time. An epoch is a HEAD counter, not a time.

After:
- **`created_ms`** on [`CopyStatus`]: the clock of the commit that created the copy, read by
  `Engine::now_ms` (M148's clock, fixed in tests by `set_clock_for_test`). A pause or resume
  does not change it, and neither does a repeated `POST` answered with the existing copy.
- **`ended_ms`**: the clock of the commit that ended it, for every end. That is the finishing
  commit of a success, the commit that ends a `source_not_found` or `rebuild_refused` failure,
  and a cancel. It is `None` while the copy is running or paused.
- **HEAD** gains one trailing section after M168's `reindexes`: `copy_times:
  BTreeMap<String, (u64, u64)>`, mapping each name to `(created_ms, ended_ms)`, where an
  `ended_ms` of `0` means not ended. The encode filter alone owns "never outlives its copy": a
  name is kept only while `once` (of a replication that exists) or `copies` holds it, as `exports` and `reindexes` are. A drop
  and a branch, which remove `copies[dest]`, rely on the filter and do not touch the record. A
  non-empty section forces the reindexes' and exports' counts (`0` when none), as every later
  section does, and the decoder's final end check moves after it.
  - A HEAD without the section encodes byte-for-byte as M168's.
  - A copy created before M194 has no record, so its status reports `None` for both times.
- **Who writes the record:**
  - A copy's create, in `create_inner`'s commit, writes `(now, 0)`. A replication's create
    writes nothing: it removes `copies[dest]` and `once` already, so the filter never encodes a
    stale record for its name. The `POST` answer of a new copy reports the `created_ms` its
    winning commit attempt wrote, captured from that attempt as `matched` is, never a second
    clock read.
  - Each commit that ends a copy sets `ended_ms` to its own clock, `max(now, 1)` so that an end
    is never read as `0`. That covers the finishing commit in `replicate_inner`, the same
    commit's ended (`gone`) copies, and `cancel_replication`. It stamps only an existing
    record: a copy created before M194 ends with no record and reports `None` for both times.
  - The record is written by a commit the operation already makes, so this adds no request
    and no commit.
- **Server:** the copy status (`GET` and `POST /v1/indexes/{dest}/copy`) adds `created_at` and
  `ended_at`. Each is an RFC 3339 string in milliseconds, the format the index summary's
  `created_at` and `updated_at` use (M148, `rfc3339_ms`), or `null`.

Not delivered:
- Progress (segments or bytes done). A copy finishes in one commit, so durable progress would
  need a HEAD write per sync step. That is a commit that scales with elapsed time, and is left
  for a design of its own.
- Times for other operation kinds.
- A clock bound. The times are each writer's wall clock, as M148's are, so a skewed node
  records a skewed time.

Not changed: what a copy does, its states and codes, every other HEAD section, and the
replication status route.

## Acceptance criteria
1. **Success** (engine): with the clock fixed at 1,000 the copy is created. Its status reports
   `created_ms = Some(1000)` and `ended_ms = None`. It is then paused at 2,000 and resumed at
   3,000, and its status still reports 1,000 and `None`. A sync at 5,000 finishes it:
   `succeeded`, with `created_ms = Some(1000)` and `ended_ms = Some(5000)`. A repeated `POST` at
   6,000 answers the same two times.
2. **Every other end** (engine): a cancel at 7,000 reports `ended_ms = Some(7000)`. A reindex
   whose seal is refused, synced at 8,000, reports `failed` with `rebuild_refused` and
   `ended_ms = Some(8000)`. A copy whose source is dropped after the create
   (`source_not_found`), synced at 9,000, reports `ended_ms = Some(9000)`. Each keeps its own
   `created_ms`. A copy whose record was removed from HEAD (as one created before M194 has
   none) and is then cancelled reports `None` for both times. A copy cancelled with the clock
   at 0 reports `ended_ms = Some(1)`.
3. **A record never leaks**:
   - (engine) A copy into a name whose earlier copy failed reports its own
     `created_ms` and `ended_ms = None`.
   - (engine) A replication created into a name whose earlier copy failed leaves no
     `copy_times` record for that name in HEAD as read back.
   - (unit) A HEAD given 65 recorded copies, each with a record, encodes the record of none
     that `record_copy` evicted past `COPIES_RETAINED`.
4. **HEAD** (unit): the section round-trips alone and after every earlier one. A HEAD whose names
   have no record encodes byte-for-byte as M168's. A HEAD cut short never decodes the section.
   A record for a name neither `once` nor `copies` holds is not encoded.
5. **Server:** a copy's `POST` answer and its `GET` report `created_at` as an RFC 3339
   millisecond time within the wall-clock bounds of the `POST`, and `ended_at` as `null` while
   running. After the sync, `ended_at` is such a time between `created_at` and the read. The
   server has no clock seam, so the bounds stand in for a fixed clock.
6. **Gates:** fmt, clippy, suite, coverage 95/95, mutants.

## Test plan
| AC | Test | Mutation it catches |
|---|---|---|
| 1 | engine `a_copy_reports_when_it_was_created_and_ended` | `created_ms` reset by resume or by a repeated `POST`; the not-ended `0` reported as a time; the finish not stamped |
| 2 | engine `every_copy_end_is_stamped` | a cancel, a refused rebuild or a missing source not stamped; an end inserting a record where none was; an end at clock 0 read as not ended |
| 3 | engine `a_copy_time_never_outlives_its_copy`, `head.rs` unit `evicted_copy_times_are_not_encoded` | the encode filter dropped; a copy's create not writing a fresh record |
| 4 | `head.rs` unit `copy_times_round_trip_after_reindexes` | section written when empty; a name kept without its record; earlier counts not forced |
| 5 | server `a_copy_status_has_wall_clock_times` | fields missing or not RFC 3339 |

## RA budget
No request is added. The record is written by the create, sync, and cancel commits that already
happen. HEAD grows by one 4-byte count, and one name and 16 bytes per copy in flight or retained, which is at most
`COPIES_RETAINED` (64) retained. Nothing here is on a user-facing path.

## Risks
- A process older than M194 refuses a HEAD carrying the section (`CorruptHead`). Upgrade every
  node before creating copies, as for every section since M22.
- Wall-clock skew between writers, as for M148.

## Tasks
- M194.1: the engine field, HEAD section, stamps at create and at every end, and status.
- M194.2: the server fields, the docs, and the DELIVERY row.
