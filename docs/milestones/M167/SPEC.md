# M167: an export is a copy that finishes read-only, and a restore is proven independent

Spec review round 1 failed (three majors: nothing recorded the flag asked for once a copy
ended; the flag set outside the finishing CAS was not caught; the drill never removed the
export before checking the restore). All three are addressed below, with the minors; round 2
passed.

## Serves
Workstream H of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "export
documents from a fixed retained snapshot, including IDs/types/schema, so a restore into a fresh
tenant produces the same exact-query/filter results and row digest", where "a concurrent
writer/GC cannot make an acknowledged export silently inconsistent".

## Delta
M166's copy already materializes one source HEAD's folded state (segments, delete vectors and
schema, under `dest`'s own keys, in any tenant or store the principal may read) and records
that HEAD's epoch as `source_epoch`. M22's sync re-reads the source when a segment is reaped
mid-copy, so what a sync commits is always one source epoch, never a mix. What it lacks as an
export: the copy is writable, so nothing keeps it equal to the snapshot it names; and no test
proves a restore from it is independent of the source. After:
- **`"read_only": true`** in a copy's body (M166's route, default `false`): the finishing
  commit also adds `dest` to M149's `read_only`, in the same CAS. Writes, deletes and the drop
  are then refused as M149 refuses them, until `PATCH` clears the flag. This is an export.
- The flag asked for is kept in one new trailing HEAD section, after M166's: `exports`, the
  names whose copy asked `read_only`. It lives as long as the copy's record does: encode keeps
  a name only while `once` or `copies` holds it, so it goes with the outcome (a drop of `dest`
  removes both; a replication, copy or branch into `dest` replaces both, and a writable copy
  clears it, as `create_inner` does `once`). A HEAD without it encodes byte-for-byte as before.
- **Status**: M166's status gains `read_only`, the flag asked for, in every state. A
  repeated POST matches only with the same flag asked (so a `read_only: true` POST after a
  `PATCH` cleared the flag still answers `200`); a different flag is answered as another
  source is (`409`).
- **Restore** is M166's copy whose source is the export, into any tenant or store: no new
  route. A restore with `read_only` absent is writable.
- **Docs**: `docs/api-migrations.md` names export (a read-only copy) and restore (a copy of
  it), states that an export holds the source's **folded** rows at `source_epoch` (unfolded
  writes are not in it: fold first, `POST /v1/admin/fold`), that `source_epoch` is the
  source HEAD the sync copied, which may be later than the POST, and that pinning a past epoch
  is not offered.
Not delivered from H: a past `as_of` epoch as the snapshot (`Head::as_of` resurrects segment
refs without rows, sidecars or lengths); a document-format (NDJSON) export; RPO/RTO under
store loss; encryption (I). Not changed: queries, the copy's syncs, and every format but
HEAD's.

## Acceptance criteria
1. A copy with `read_only: true` is `succeeded` and read-only after one sync: a write, a delete
   and the drop are refused with M149's errors, and its status and summary say `read_only`;
   after `PATCH` clears the flag a write folds. A copy without the flag is writable.
2. The flag is set by the finishing commit itself: after the sync, dest's HEAD epoch is the
   outcome's `epoch` (no later commit) and `read_only` holds `dest`; with the finishing commit
   made to lose once, no HEAD read inside the interference holds a `succeeded` outcome.
3. Restore drill (server). Server A's primary store `main` holds source tenant S; server B's
   primary store is `far`, with `main` as a source store. On B, an export copies S's index
   (two folded segments, a delete vector, typed attributes, a text field) into tenant X. On A,
   S's index is dropped and collected until its segment objects are gone. On A, a restore
   copies X's export from `far` into a fresh tenant R. Then, on B, the export's flag is cleared
   and it is dropped and collected until its segment objects are gone, and A forgets `far`.
   The row digest (every row's id, attributes and vector, paged by `rank_by` id) of S before
   its drop, of the export before its drop and of R are equal, and so are R's dense top-k,
   filter, text and id-lookup answers against S's taken before the drop. R takes a write
   that folds.
4. Mid-copy GC (engine): the source compacts, collects and takes a further write and fold
   between the export's source read and its first segment GET. The export succeeds, and its
   rows equal the source's at its recorded `source_epoch` (digest), never a mix of two epochs.
5. A failed or cancelled read-only copy's status says `read_only` and no index is read-only
   under its name; a later writable copy into the same name is writable and its status says
   not; a drop of an export removes its section entry.
6. A repeated POST with the same source and flag is `200`, after a `PATCH` clear too; with the
   other flag it is `409`.
7. HEAD round-trips the section after every earlier one; a HEAD without it encodes
   byte-for-byte as before. Gates: fmt, clippy, suite, coverage 95/95, mutants.

## Test plan
| AC | Test | Mutation it catches |
|---|---|---|
| 1 | server `an_export_is_read_only_until_cleared` | flag not applied at finish; status from the current flag |
| 2 | engine `an_export_is_read_only_in_its_finishing_commit` | flag set by a second commit after the finish |
| 3 | server `an_export_restores_into_a_fresh_tenant` | schema or delete vectors not copied; restore reading the export's objects |
| 4 | engine `an_export_is_one_source_epoch` | `source_epoch` recorded from the first read, not the committed one |
| 5 | engine `a_read_only_copy_that_ends_otherwise_says_so` | flag set at failure or cancel; writable copy not clearing the entry; entry kept at drop |
| 6 | engine `a_repeated_export_matches_only_its_flag` | flag ignored by `same_copy`, or compared with the current flag |
| 7 | `head.rs` unit test `exports_round_trip_after_copies` | section written when empty; a name kept without its record |

## RA budget
Unchanged: a POST, a sync and a status are M166's. The finishing commit adds one name to
`read_only` in the CAS it already makes. HEAD grows by one name per read-only copy in flight.

## Risks
- A process older than M167 refuses a HEAD carrying the section (`CorruptHead`): upgrade every
  node before offering exports, as for every section since M22.
- An export holds folded rows only; a caller who does not fold first exports less than it
  wrote. Documented, and `source_epoch` names exactly what it holds.
- Clearing an export's flag makes it writable; nothing then keeps it equal to `source_epoch`.

## Tasks
- M167.1: HEAD section, finishing and ending commits, repeated-POST match.
- M167.2: server body, status field, docs, restore drill.
