# M149: an index can be made read-only

Spec review round 1 failed with one major: `set_read_only` did not say which existence
rule it used, and the summary's (rows held unfolded count) would have answered a flag
that `Head::encode` then drops. Existence is now HEAD's, with `409 index_not_folded` for
a name held only unfolded. Its three minors are folded in: the per-kind door window, the
quarantine object in the RA budget, and the export's `reason`. Round 2 passed with one
minor, folded in: a refused request's earlier rows are served by its own process until
the fold that sets them aside.

## Serves
Workstream G of [the delivery plan](../../comparisons/turbopuffer-next-plan.md):
"Read-only rejects new document/schema mutations from every server and copy/job entry
point. Define whether previously accepted jobs drain or cancel; indexing already accepted
data must remain correct. A paused writer cannot publish past a newer flag without
satisfying its fencing rules. Branch inheritance is defined and tested." The comparison's
`read_only` row says only a replica is read-only today
([comparison](../../comparisons/turbopuffer-2026-10-25.md)).

## Delta
Today a replica (M22) is the only index that refuses writes: the write door refuses by
the replica names the process last read, re-reading HEAD once before a refusal stands,
and the fold drops a replica's rows that passed a door which had not read the replication,
counting them on it. There is no flag an owner can set on an ordinary index.

After:
- **HEAD gains one trailing section**, `read_only`: a set of index names. It follows
  M148's `times` and forces every optional section before it. An empty set is never
  written, so a HEAD with no read-only index encodes byte for byte as M148's. When the set
  is non-empty and `times` is empty, `times` is written in its marker form with a count of
  0. The decoder reads the set if bytes remain after `times`, then the strict end check.
  `Head::encode` writes only names that exist (a key of `indexes`, `schema_rejects` or
  `quarantine`, M148's rule), so the flag never outlives its index and a name created
  again is writable.
- **Setting it.** `PATCH /v1/indexes/{index}` (scope `write`) with `{"read_only": bool}`
  (no other field; an unknown field is refused). It answers `200 {"epoch", "read_only",
  "cost"}`. One HEAD read and, when the value changes, one CAS commit; setting the value
  it already has commits nothing and answers the HEAD's epoch. Existence is HEAD's, the
  rule `Head::encode` filters by (a key of `indexes`, `schema_rejects` or `quarantine`),
  decided on each attempt, so a flag answered is a flag written. A name HEAD does not
  hold is `404 index_not_found`, except that one this process holds rows for unfolded
  (which its summary reports, `unfolded: true`) is `409 index_not_folded`: retry after
  the fold. `409 replica_read_only` for a replica (it is read-only by its replication,
  and cannot be made writable here). Engine:
  `Engine::set_read_only(index, bool) -> Result<Epoch, EngineError>`, retrying on CAS
  loss like every commit, deciding existence and the replica refusal on each attempt.
- **The door.** Every document mutation (upsert, patch, delete, conditional and
  by-filter operations, which all enter through the engine's write door) to an index the
  process knows is read-only is refused with `EngineError::ReadOnly(index)`, answered
  `409 index_read_only`, and writes nothing. The process knows by the read-only names of
  the newest HEAD it read, kept beside the replica names (M22) by the same rule, at zero
  requests; a refusal re-reads HEAD once before it stands, so a flag cleared by another
  process is seen. A process that has not read the flag accepts the write. ⚠️ A request
  carrying several operation kinds passes the door once per kind, as for a replica today:
  if a fold in between teaches the process the flag, the request answers `409
  index_read_only` after its earlier kinds were buffered, and those rows are set aside by
  the fold below: kept, counted, and not served after that fold (until it, this process
  serves them as buffered rows).
- **The fold sets aside, never drops.** A fold attempt that reads a HEAD naming an index
  read-only takes that index's rows out of the batch before the reject pass, every kind
  (rows, tombstones, patches), and puts them in the index's quarantine (M25) with their
  count added to its `schema_rejects`, as a schema reject is. So rows a stale door
  accepted are kept, exportable through the quarantine route, and reported as rejected
  rows in the summary; under applied writes (M129) their operation's verdict is
  rejected, as for a schema reject. The quarantine export derives each row's `reason`
  from the schema, so a set-aside row that fits its schema exports with `reason: null`;
  HEAD does not record why a row was set aside. The fold still commits, so the lane's watermark moves
  past them.
- **Jobs accepted before the flag are cancelled, not drained.** Rows acknowledged before
  the flag's commit and not yet folded are folded after it, and so are set aside as
  above. Rows folded before the flag stay indexed and served. Clearing the flag does not
  replay set-aside rows; they stay in the quarantine until discarded.
- **Fencing.** The fold, compaction and every commit decide on the HEAD each CAS attempt
  read. A writer paused before the flag's commit loses its CAS to it and, on its next
  attempt, reads the flag and sets its rows aside. No lock or lease.
- **Other entry points.**
  - Drop (`DELETE /v1/indexes/{index}`, the fold's drop): refused with `409
    index_read_only` while the flag is set; clear it first.
  - Create and branch into a read-only name: already refused, because the name exists.
  - Branch from a read-only index: allowed, and the destination is writable (the flag is
    not inherited; a branch is a new index). Replication from a read-only source:
    allowed; the replica is read-only by its replication as before.
  - Compaction, GC, quarantine export and discard, and `warm`: unchanged, allowed.
    Compaction rewrites already-indexed data and is not a mutation.
  - Queries: unchanged.
- **The summary** adds `"read_only": bool` (true for a replica as well), from the one HEAD
  it already reads.

Not changed: query behaviour; the request count of every route for an index that is not
read-only, and of every write whose door does not refuse; HEAD's other sections; M148's
times (a set-aside fold writes the index, so `updated_at` moves, as for any fold of
rejected rows). Mixed versions: an M148 node refuses a HEAD carrying the section as
corrupt (its decoder requires the end after `times`), and so does a node from M132 to
M147; since the section is written only while some index is read-only, every node of a
deployment runs M149 before the first flag is set. That is the operator's step, as for
M148. A node from M129 to M131 drops the section when it re-commits, clearing the flag.

## Acceptance criteria
1. A flag set through the API is reported by the summary and survives restart (a second
   `Api` over the same store); clearing it does the same.
2. While set, each of upsert, patch, delete, `patch_by_filter`, `delete_by_filter` and a
   conditional write is `409 index_read_only` from the process that set it and from a
   second process that has read HEAD since, and the store's PUT count is unchanged. A
   drop is `409 index_read_only`.
3. Rows a process accepted before it read the flag, and rows acknowledged before the
   flag's commit, are set aside by the next fold: the index's served rows are unchanged,
   its `rejected_rows` grows by their count, and its quarantine holds them. Under applied
   writes their verdict is rejected.
4. A fold whose CAS loses to the flag's commit sets aside on its retry: nothing it held
   is served.
5. After clearing the flag, writes are accepted and folded, and the set-aside rows stay
   in the quarantine only.
6. A branch from a read-only index is writable; a drop after clearing succeeds and a
   re-creation of the name is writable; a replica answers `read_only: true` and
   `PATCH` on it is `409 replica_read_only`; `PATCH` on an absent name is 404, and on a
   name this process holds only unfolded is `409 index_not_folded` with no write.
7. HEAD round-trips with `read_only` beside every combination of absent and present
   `keys_since` and `times`; a HEAD with no read-only index encodes byte for byte as
   M148's; the flag of a name that does not exist is not written.
8. Setting the flag costs one HEAD read and one CAS, and setting the value it has costs
   one HEAD read and no write. A write to an index that is not read-only costs what it
   did before.
9. Gates: clippy, the workspace suite, coverage 95/95, mutation of the changed code.

## Test plan
- Engine unit: `read_only_round_trips_beside_times` (7) kills a section written when
  empty, not forcing the sections before it, or keeping a flag for an absent name.
- Engine: `a_read_only_index_refuses_every_write_kind` (2), `a_stale_door_is_set_aside`
  (3), `a_fold_losing_to_the_flag_sets_aside` (4: a drop of another index is a fold that
  carries the pending rows, and its interference hook commits the flag between its read
  and its CAS),
  `clearing_the_flag_writes_again` (5), `read_only_lifecycle` (6: branch, drop, re-create,
  replica) kill each refusal, the fold's set-aside, and the flag kept past its index.
- Server: `read_only_survives_restart_and_refuses_writes` (1, 2, 8) and
  `patch_refusals` (6) kill a missing route, the wrong status or code, and a write on a
  no-op set.

## RA budget
Unchanged for queries and for writes to indexes that are not read-only. A refused write
costs at most one HEAD read. A flag set or cleared costs one HEAD read and one CAS. HEAD
grows by one name per read-only index. A fold that sets rows aside writes one quarantine
object per read-only index it carries rows for, as a fold of schema rejects does, and
stamps that index's M148 time. No new kind of object, no request per record, no LIST.

## Risks
- A write entry point that bypasses the engine's door. All document mutations go through
  it today; the fold's set-aside catches anything that does not.
- A stale process acknowledges a write the fold then sets aside. That is the defined
  behaviour (3) and the rows are kept; a caller that needs a verdict uses applied writes.

## Tasks
- M149.1: the HEAD section, `set_read_only`, the door, the fold's set-aside and the drop
  refusal, with engine tests.
- M149.2: the route, the summary field, server tests; docs (api-migrations, DELIVERY,
  comparison).
