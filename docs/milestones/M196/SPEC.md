# M196: a copy or export of a past epoch (`as_of`)

## Serves
Workstream H of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Export
documents from a fixed retained snapshot ... A concurrent writer/GC cannot make an acknowledged
export silently inconsistent", the "past `as_of` epoch" left pending by [M167](../M167/SPEC.md).

## Delta
Today a copy (M166), export (M167) or reindex (M168) holds the source's folded state at the
source HEAD its sync read. M167's docs say: "A past `as_of` epoch cannot be exported".

After:
- **`as_of`** on a copy's create: `POST /v1/indexes/{dest}/copy` takes `"as_of": <epoch>`,
  with or without `read_only`. The engine's `create_copy_with(dest, source, from,
  CopyOptions { read_only, reindex, as_of })` is what `create_copy`, `create_export` and
  `create_reindex` now call.
- **What it holds:** the source index as a query `as_of` that epoch reads it. Its segments
  and delete vectors are those of `Head::as_of` (M9f/M9c). The schema is the one the index
  had then: `dropped`'s for an epoch before a drop (as a query `as_of` scores), else the
  present's. So an index dropped since, but still within GC's horizon, can be restored.
- **Counts.** A segment or delete vector `Head::as_of` resurrects from the graveyard carries
  `rows: 0` and no length. An `as_of` copy takes each segment's rows from its footer and its
  length from its bytes, and each delete vector's count from its bytes, all read by the copy
  itself. So its index stats report the epoch's live documents. A copy without `as_of` is
  unchanged, byte for byte.
- **Refused at the create**, as a query `as_of` is:
  - an epoch after the source's present: `time_travel_horizon` (400);
  - an epoch below the source's reap horizon: `time_travel_horizon` (400);
  - a source that is itself a replica: 409 `replica_no_history`, as a query `as_of` it is;
  - no segment of the source index at that epoch: `source_not_found`;
  - `as_of` with `schema`: 400 `invalid` ("reindex the export"). A reindex of a past epoch is
    an `as_of` export, then a reindex of it.
  - `as_of` on a replication create (`POST /v1/indexes/{dest}/replication`): 400, as
    `read_only` is.
- **Ended typed at a sync.** If the epoch is no longer answerable when a sync reads the
  source, or a remap re-reads it, the copy ends `failed` with the new code
  `epoch_unavailable`. That is a GC whose horizon passed it, or a source that became a
  replica. The copy is never partly published, and never published as another epoch: a
  segment GC deleted before its horizon commit reads as missing, and the copy stays running
  until a later read sees the horizon.
- **Status:** `CopyStatus::as_of`, and the server's copy status `"as_of"` (epoch or `null`).
  A succeeded copy's `source_epoch` is the `as_of` epoch. A repeated `POST` matches a copy
  only with the same `as_of`, as it matches `read_only` and `schema`.
- **HEAD** gains one trailing section after M194's `copy_times`: `copy_as_of:
  BTreeMap<String, u64>`. The encode filter is the one `copy_times` uses (`once` of an existing
  replication, or `copies`). A non-empty section forces the earlier three counts (exports,
  reindexes, copy times). A copy's create, in its `once` branch, writes the epoch, or removes a
  stale one when it asks none. A HEAD without the section encodes byte for byte as M194's.
- **Sync state:** `SyncState` reads each copy's `as_of` with the plan, as it reads `once`.

Not delivered: a document-format export, measured RPO/RTO, a reindex of a past epoch in one
operation, and a hold that stops GC passing an epoch a copy is reading. A copy past the horizon
fails typed instead.

Not changed: a copy, export or reindex without `as_of`; every other HEAD section; queries.

## Acceptance criteria
1. **Export of a past epoch** (engine): a source is folded at epoch `E`. After `E` it has a
   row deleted (a new delete vector), a row replaced, a compaction that buries `E`'s segments,
   and then a drop. An export `as_of E` succeeds with `source_epoch = Some(E)` and
   `as_of = Some(E)`. It answers the exact query and the filter that the source answered
   `as_of E` before the drop, with the same ids, and the same row digest. Its stats report
   `E`'s live documents and segments.
2. **Refused at the create** (engine): an epoch after the present and one below the horizon
   are `TimeTravel`, a replica source is `NoHistory`, an epoch with no segment of the index is
   `SourceNotFound`, and `as_of` with a reindex is `Invalid`. None writes HEAD.
3. **GC passes it** (engine): a copy `as_of E` is created, then a GC whose horizon is past `E`
   runs, then a sync. The copy ends `failed` with `epoch_unavailable` and `ended_ms` set, and
   `dest` holds no segment. A copy without `as_of` from the same source, synced by the same
   sync, succeeds.
4. **A record never leaks** (engine): a repeated `POST` with the same `as_of` answers the
   running copy, and one with another `as_of` is refused `Exists`. A copy without `as_of` into
   the name of a failed `as_of` copy reports `as_of = None`, and holds the present.
5. **HEAD** (unit): the section round-trips alone and after every earlier one. A HEAD with no
   `as_of` encodes byte for byte as M194's. A HEAD cut short never decodes the section. A record
   for a name neither `once` nor `copies` holds is not encoded. The failure code round-trips.
6. **Server:** a `POST` with `as_of` reports `"as_of"`, and after the sync `succeeded` with that
   `source_epoch`. A copy without it reports `"as_of": null`. A future epoch is 400
   `time_travel_horizon`. `as_of` with `schema` is 400, and `as_of` on a replication create is
   400. A failed copy reports `"failure": "epoch_unavailable"`.
7. **Gates:** fmt, clippy, suite, coverage 95/95, mutants.

## Test plan
| AC | Test | Mutation it catches |
|---|---|---|
| 1 | engine `an_export_holds_a_past_epoch` | the present copied; resurrected rows or delete counts left 0; the present's schema after a drop |
| 2 | engine `a_past_epoch_is_refused_as_a_query_refuses_it` | any refusal dropped; the present's fingerprint checked |
| 3 | engine `a_reaped_epoch_ends_a_copy_typed` | the reap treated as transient; a present copy published instead |
| 4 | engine `an_as_of_never_outlives_its_copy` | `as_of` left out of the match; the stale epoch kept by a create |
| 5 | `head.rs` unit `copy_as_of_round_trips_after_copy_times` | section written when empty; earlier counts not forced; filter dropped |
| 6 | server `a_copy_takes_an_as_of_epoch` | field not parsed, not reported, or not refused |

## RA budget
No request is added for a copy without `as_of`. An `as_of` copy reads the same objects a copy
does, and the counts come from bytes it already fetched. A remap re-reads one source HEAD, as
today. HEAD grows by one 4-byte count and one name and 8 bytes per `as_of` copy in flight or
retained, at most `COPIES_RETAINED` (64) retained. Nothing is on a user-facing query path.

## Risks
- A process older than M196 refuses a HEAD carrying the section or the new failure code
  (`CorruptHead`). Upgrade every node before creating `as_of` copies.
- GC can delete a segment before it commits its horizon. A copy reading that window sees it
  missing and remaps. After `MAX_STALLS` remaps it reports a transient failure, and a later sync
  reads the horizon and ends it typed. The tests drive the end, not the window.

## Tasks
- M196.1: the engine options, the view, the counts, the HEAD section, the failure code, and
  the status.
- M196.2: the server field, the docs, and the DELIVERY row.
