# M196: a copy or export of a past epoch (`as_of`)

Spec review round 1 failed on one blocker and three majors: the remap read the present HEAD;
the create, the sync's fingerprint and the schema read the present; a GC deleting before it
commits its horizon could pass a copy unseen; and the copy's `len` and `dicts` were undefined.
Each is addressed below under **The view**, **Counts** and **GC**. Round 2 failed on one major:
where the re-read before publishing sits. It is now in every commit attempt, after the test's
interference point. Round 3 passed. Amended during implementation: see **Counts**, the
remap test `a_remapped_past_epoch_keeps_its_counts` added under AC 1, and the failure code's
string checked by a server unit test.

## Serves
Workstream H of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Export
documents from a fixed retained snapshot ... A concurrent writer/GC cannot make an acknowledged
export silently inconsistent", the "past `as_of` epoch" left pending by [M167](../M167/SPEC.md).
Partly: a hold that GC honours is still missing (see Risks).

## Delta
Today a copy (M166), export (M167) or reindex (M168) holds the source's folded state at the
source HEAD its sync read. M167's docs say: "A past `as_of` epoch cannot be exported".

After:
- **`as_of`** on a copy's create: `POST /v1/indexes/{dest}/copy` takes `"as_of": <epoch>`,
  with or without `read_only`. The engine's `create_copy_with(dest, source, from,
  CopyOptions { read_only, reindex, as_of })` is what `create_copy`, `create_export` and
  `create_reindex` now call.
- **The view.** Every place a copy reads a source HEAD reads, for an `as_of` copy, its view:
  `Head::as_of(E)` (M9f/M9c's segments and delete vectors), with `schemas[index]` replaced by
  the schema the index had at `E` (`schema_recorded_at`, as a query `as_of` scores; removed
  when none is recorded). That is the create's existence check, the sync's fingerprint (so
  `known`, `gone` and `current` too), `mirror`'s segments, delete vectors and schema, and
  `mirror`'s remap: the fresh HEAD it re-reads is viewed at `E` too, and the `still` set comes
  from that view. A view that fails (`TimeTravel`) at a sync or a remap ends the copy.
- **Counts.** A ref the view resurrects carries `rows: 0`, no `len` and no `dicts`. A copy sets
  each ref's `rows` from its segment's footer and `len` from its byte count, both read from the
  bytes it fetched, and keeps them across a remap with the copied key. Each delete vector's
  count is decoded from the bytes it fetched, and that count, not the view's, is kept, including
  across a remap. A resurrected ref's `dicts` stays `None` ("unknown asks", as any ref older
  than M32); a live one keeps its source's. *(Amended in implementation: this holds for every
  copy, not only an `as_of` one. For a present copy the bytes say what HEAD says, so the guard
  that limited it to `as_of` was an equivalent mutant; only a ref older than M45 gains a
  `len`.)*
- **Refused at the create**, as a query `as_of` is:
  - an epoch after the source's present, or below its reap horizon: `time_travel_horizon` (400);
  - a source index that is a replica: 409 `replica_no_history`, as a query `as_of` it is;
  - no segment of the source index in the view: `source_not_found`;
  - `as_of` with `schema`: 400 `invalid` ("reindex the export"). A reindex of a past epoch is an
    `as_of` export, then a reindex of it;
  - `as_of` on a replication create: 400, as `read_only` is.
  None of these writes HEAD.
- **GC.** At a sync or a remap, a view that fails ends the copy `failed` with the new code
  `epoch_unavailable`, in the commit that would have published it. GC deletes before it commits
  its horizon, so **every commit attempt** that would publish an `as_of` copy checks the view
  again, after `replicate_with_interference_for_test`'s interference point: of the attempt's own
  dest HEAD when the source names no store and this tenant (no request), else of a fresh read of
  the source's HEAD. A view that fails ends the copy instead, in that attempt's commit. A segment GC already
  deleted reads as missing, and the remap stalls until `MAX_STALLS` (a transient failure,
  retried).
- **Status:** `CopyStatus::as_of`, and the server's copy status `"as_of"` (epoch or `null`). A
  succeeded copy's `source_epoch` is `E`. A repeated `POST` matches a copy only with the same
  `as_of`, as it matches `read_only` and `schema`.
- **HEAD** gains one trailing section after M194's `copy_times`: `copy_as_of: BTreeMap<String,
  u64>`. Its encode filter is `copy_times`' (`once` of an existing replication, or `copies`). A
  non-empty section forces the earlier three counts. A copy's create, in its `once` branch,
  writes the epoch or removes a stale one. The in-memory map may hold names the filter drops, as
  `copy_times` may. A HEAD without the section encodes byte for byte as M194's. The failure code
  `epoch_unavailable` is `2` in the `copies` section.
- **Sync state:** `SyncState` reads each copy's `as_of` with the plan, as it reads `once`.

Not delivered: a document-format export, measured RPO/RTO, a reindex of a past epoch in one
operation, and a hold that stops GC passing an epoch a copy reads (see Risks).

Not changed: what a copy, export or reindex without `as_of` holds (its refs now take `rows` and
`len` from the copied bytes, which equal what HEAD says); every other HEAD section; queries.

## Acceptance criteria
1. **Export of a past epoch** (engine): a source is folded at epoch `E`. After `E` it has a row
   deleted (a new delete vector), a row replaced, a compaction that buries `E`'s segments, and
   then a drop. An export `as_of E` succeeds with `source_epoch = Some(E)` and `as_of = Some(E)`.
   It answers the exact query and the filter the source answered `as_of E` before the drop, with
   the same ids and row digest. Its stats report `E`'s live documents and segments, and each of
   its HEAD refs has the footer's `rows` and the object's byte length as `len`.
2. **Refused at the create** (engine): an epoch after the present and one below the horizon are
   `TimeTravel`, a replica source is `NoHistory`, an epoch with no segment of the index is
   `SourceNotFound`, and `as_of` with a reindex is `Invalid`. HEAD's epoch is unchanged by each.
3. **GC passes it** (engine): a copy `as_of E` is created, then a GC whose horizon is past `E`
   runs, then a sync. The copy ends `failed` with `epoch_unavailable` and `ended_ms` set, and
   `dest` holds no segment. A copy without `as_of` from the same source, synced by the same sync,
   succeeds. A copy whose GC commits between its GETs and its re-read (driven by
   `replicate_with_interference_for_test`) ends the same way.
4. **A record never leaks** (engine): a repeated `POST` with the same `as_of` answers the running
   copy, and one with another `as_of` is refused `Exists`. A copy without `as_of` into the name of
   a failed `as_of` copy reports `as_of = None`, and holds the present.
5. **HEAD** (unit): the section round-trips alone and after every earlier one. A HEAD with no
   `as_of` encodes byte for byte as M194's. A HEAD cut short never decodes the section. A record
   for a name neither `once` nor `copies` holds is not encoded. The failure code round-trips.
6. **Server:** a `POST` with `as_of` reports `"as_of"`, and after the sync `succeeded` with that
   `source_epoch`. A copy without it reports `"as_of": null`. A future epoch is 400
   `time_travel_horizon`. `as_of` with `schema` is 400, and `as_of` on a replication create is
   400. A copy ended `epoch_unavailable` reports `"code": "epoch_unavailable"`.
7. **Gates:** fmt, clippy, suite, coverage 95/95, mutants.

## Test plan
| AC | Test | Mutation it catches |
|---|---|---|
| 1 | engine `an_export_holds_a_past_epoch`, `a_remapped_past_epoch_keeps_its_counts` | the present copied; the remap or `still` read from the present; resurrected rows, len or delete counts left as the view's; the present's schema after a drop |
| 2 | engine `a_past_epoch_is_refused_as_a_query_refuses_it` | any refusal dropped; the present's fingerprint checked |
| 3 | engine `a_reaped_epoch_ends_a_copy_typed` | the reap treated as transient; the re-read before publishing dropped |
| 4 | engine `an_as_of_never_outlives_its_copy` | `as_of` left out of the match; the stale epoch kept by a create |
| 5 | `head.rs` unit `copy_as_of_round_trips_after_copy_times` | section written when empty; earlier counts not forced; filter dropped |
| 6 | server `a_copy_takes_an_as_of_epoch`, unit `an_unavailable_epoch_has_its_code` | field not parsed, not reported, or not refused; the failure code's string |

## RA budget
No request is added for a copy without `as_of`. An `as_of` copy reads the objects a copy does,
plus, for a source in another store or tenant, one source HEAD read per commit attempt that
would publish it (none for a source in its own store and tenant). Its counts come from bytes it already
fetched. HEAD grows by one 4-byte count, and one name and 8 bytes per `as_of` copy in flight or
retained, at most `COPIES_RETAINED` (64) retained. Nothing is on a user-facing query path.

## Risks
- A process older than M196 refuses a HEAD carrying the section or the failure code
  (`CorruptHead`). Upgrade every node before creating `as_of` copies.
- **The GC window.** GC deletes before it commits its horizon. A copy whose GETs and re-read
  both fall inside one GC pass's delete-to-commit window can read a segment whose sidecars were
  already deleted, and take them as never written, as M166 does. Closing that needs a hold that
  GC honours, which is not delivered. The re-read narrows the window to one GC pass.

## Tasks
- M196.1: the engine options, the view, the counts, the HEAD section, the failure code, and the
  status.
- M196.2: the server field, the docs, and the DELIVERY row.
