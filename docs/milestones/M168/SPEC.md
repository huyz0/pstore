# M168: a reindex is a copy that rebuilds an index's text and regex options

Spec review round 1 failed (five majors: AC 4 never forced the remap its mutation needs; the
rebuilt segment's ref must come from the seal, not the source; the source must be read through
the copy's own store, not `dest`'s; a refused seal retried on every sync scales with elapsed
time; the seal's written names were never buried). All five are addressed below, with the
minors. Round 2 failed on one major (AC 6 named no reachable trigger); it now names one, and
which errors end a copy. Round 3 passed.

## Serves
Workstream G of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Add/drop/
index-option changes have an explicit support matrix. Backfill publishes a version
atomically; a failed worker does not expose half an index. Incompatible type, metric or
analyzer changes use migration rather than unsafe in-place reinterpretation." Budget: "Backfills
scale with bytes, are cancellable/retryable".

## Delta
Today an index's analyzer, further text fields and regex set are fixed by its first fold: a
write declaring another set is `schema_conflict`, whose message names a reindex "by copying
the rows into a new index" (M14, M15.2, M155). No route does that without the client re-writing
every row. M166's copy moves an index's folded state into a new index as one operation with a
status, but it copies segment bytes, so their postings and sketches are the source's.

After:
- **`"schema"`** in a copy's body (M166's `POST /v1/indexes/{dest}/copy`): the same map a write
  declares, holding only `full_text_search` and `regex` entries (a `type` of `string` or
  `[]string` is allowed beside them). It makes the copy a **reindex**: `dest`'s schema is the
  source's with
  - the primary text field's (`text`) options replaced when the map declares `text`, kept
    otherwise;
  - the further text fields replaced by exactly the others the map declares `full_text_search`
    on (so a field is added by declaring it and dropped by leaving it out);
  - the regex set replaced by exactly the names the map declares `"regex": true` on.
  Dims, metric, named-field widths, attribute types and the id kind are the source's.
- **The rebuild.** Where a copy GETs a source segment and its sidecars and PUTs them, a reindex
  GETs, through the copy's own source store (never `dest`'s), the segment, its sparse
  dictionary sidecar when the source's ref records one, and its delete vector; scans the
  segment from an in-memory store, as the fresh half does; keeps the live rows (the delete
  vector decoded by `pstore_query::deletes::decode`); and seals them under `dest`'s schema
  with the fold's and compaction's own `seal`: a new segment and new sidecars, and no delete
  vector. `dest`'s ref for it (`key`, `rows`, `dicts`, `len`) is what the seal returns and the
  live row count, never the source's: an added text field or a dropped regex set changes the
  sidecars. Every name the seal created, or may have, joins the sync's written names, on
  success and on error, so whatever no commit names is buried.
- **The rebuilt key** is the copy's (`seg/R/{epoch}-{lane}-{h}.seg`) with
  `h = fnv1a(source segment key ‖ 0x00 ‖ delete-vector key, or nothing)`, so it is never a
  byte copy's `h` (`fnv1a(source segment key)`), and a delete that lands on the source between
  remaps gives a new `h`: that segment is rebuilt rather than a stale rebuild reused. Within
  one sync a rebuild is remembered by `h` with its whole ref, and a segment whose rows are all
  deleted is remembered as giving none; either counts as progress against `MAX_STALLS`. A
  source whose every row is deleted gives `dest` its schema and no segment, as an empty index
  (M147) has.
- **The override is applied at the mirror**, to the source schema the sync copies, so a type
  the source recorded after the `POST` is carried.
- **A refused seal ends the copy** `failed` with a new code, `rebuild_refused`: only the
  seal's `EngineError::Format` (a `try_build_all_fields` or `try_finish` refusal, deterministic:
  retrying it every sync would re-read every segment for nothing). A store failure, or the
  seal's `Contended` after its refused names, stays M166's: `last_error`, retried, cancellable.
- **HEAD** gains one trailing section after M167's: `reindexes`, each name's asked options,
  kept only while `once` or `copies` holds the name, as `exports` is. A HEAD without it encodes
  byte-for-byte as before. A repeated `POST` matches only the same `schema` (absent included);
  another is `409`, as another source is.
- **Refused** with `400`: `schema` on a replication; a type declaration (`"uint64"`, ...) or
  any key but `type`, `full_text_search` and `regex`; `"regex"` on `id`; more further text fields
  than `MAX_TEXTS` (8); invalid `full_text_search` options. The options are read by the write's
  own option parser (`full_text_options`), through a wrapper with no document checks.
  Refused by the engine at create, after its one source HEAD read and before any segment read
  (`400 invalid`): a further field or regex attribute named as the source's primary text
  field when it records one, or as an attribute the source types other than string.
- **Docs**: `docs/api-migrations.md` gains the support matrix: for each change (add an
  attribute, a new attribute type, drop an attribute, the read-only flag, the primary's
  analyzer, add/drop a further text field, add/drop a regex attribute, dims, metric, an
  attribute's type, the id kind) whether it is made in place, by a reindex, or only by
  re-writing rows into a new index, with the milestone that says so.

Not delivered: an in-place reindex under the same name (an alias or swap); progress beyond the
state (bytes or segments done); a reindex that changes
dims, metric, types or the id kind. Not changed: writes, queries, folds, compaction, and every
HEAD section but the new one.

## Acceptance criteria
1. Analyzer change (engine): a source whose `text` is analyzed without stemming, holding
   "running" in one row, is reindexed with stemming on. After one sync the copy is `succeeded`,
   `dest`'s schema records stemming, a BM25 query for "run" finds the row on `dest` and not on
   the source, and the source's schema and segments are unchanged.
2. Add and drop (server): a source with further text field `title` and no regex set is
   reindexed declaring further field `body` and `"regex": true` on `tag`, leaving `title` out.
   On `dest`, a BM25 query on `body` answers, one on `title` is refused `unknown_field`, and the schema read reports `body` and the `tag` regex and no `title`. A write to `dest`
   declaring the source's set is refused `schema_conflict`; one declaring the new set is
   accepted and folds.
3. Rows (engine): the source has three folded segments: one with a delete vector, one whose
   every row is deleted, one with none. `dest`'s row digest (every row's id, attributes and
   vectors) equals the source's, `dest` has exactly two segments and no delete vector in HEAD,
   no `dest` segment's `h` is a byte copy's, each `dest` ref's `rows`, `dicts` and `len` are
   its own object's, and a dense top-k, a filter and an id lookup answer as the source does.
4. One epoch under deletes (engine): the source has segments A and B, each with live rows.
   Inside the reindex's first GET of A the source deletes a row of A and folds (a new delete
   vector on A; the old one still exists), and B's segment GET answers 404 once, forcing a
   remap. The reindex succeeds, `source_epoch` is the source's HEAD after the delete, and the
   deleted row is not in `dest`.
5. Atomic (engine): with the finishing commit made to lose once, no HEAD read inside the
   interference names a `dest` segment or schema and the status is `running`; after the retry
   it is `succeeded`. A second reindex is cancelled inside the same interference, after its
   rebuild and before its commit: it ends `cancelled`, `dest` has no index, and every object
   it wrote is in the graveyard (none leaked after a GC).
6. Refused rebuild (engine): the source's rows carry a string under an attribute whose name
   is longer than `INDEX_BUDGET` (an ordinary attribute, so the source folds). A reindex
   declaring that attribute a further text field puts the name in the seal's inline
   `TextFields` section, which `try_finish` refuses as too wide to open in one round trip.
   After one sync the copy is `failed` with `rebuild_refused`, `dest` has no index, and a
   second sync reads no source segment. Only the seal's `EngineError::Format` ends a copy: a
   seal that fails on the store (its first create made to fail once) leaves the copy `running`
   with a `last_error`, and the next sync finishes it.
7. A repeated `POST` (engine and server): the same `schema` is `200` with the same `run`; a
   different `schema`, or none, is `409`; a reindex's `schema` repeated after a plain copy into
   the name is `409`.
8. Refusals (server): each refused body above answers `400` naming the key, the server's
   before any read of the source, the engine's before any segment read.
9. HEAD round-trips the section after every earlier one; a HEAD whose names have no record
   encodes byte-for-byte as M167's. Gates: fmt, clippy, suite, coverage 95/95, mutants.

## Test plan
| AC | Test | Mutation it catches |
|---|---|---|
| 1 | engine `a_reindex_rebuilds_under_a_new_analyzer` | segment bytes copied instead of rebuilt; options not applied to the schema |
| 2 | server `a_reindex_adds_and_drops_text_and_regex_fields` | further fields merged with the source's rather than replaced; regex set not replaced |
| 3 | engine `a_reindex_keeps_live_rows_and_no_deletes` | delete vector not applied; a delete vector carried over; an all-deleted segment kept; the source's ref fields kept |
| 4 | engine `a_reindex_is_one_source_epoch_under_deletes` | the rebuilt key's hash without the delete vector's key |
| 5 | engine `a_reindex_never_reads_as_finished_partway` | schema or segments published by a commit before the finish; the seal's names not buried |
| 6 | engine `a_refused_rebuild_fails_typed` | a refused seal retried, or ended as another code; a store failure ended as `rebuild_refused` |
| 7 | engine `a_repeated_reindex_matches_only_its_schema`, server in AC 2's test | options ignored by the match |
| 8 | server `a_reindex_body_is_checked`, engine `a_reindex_naming_the_primary_is_refused` | a refusal dropped |
| 9 | `head.rs` unit test `reindexes_round_trip_after_exports` | section written when empty; a name kept without its record; the new failure code not round-tripped |

## RA budget
A `POST`, its status and a sync's HEAD reads are M166's. Per source segment a reindex makes, in
one round, a GET of the segment, of its sparse dictionary when it has one, and of its delete
vector when it has one; then the in-memory open and scan read nothing more from the blob
store; then the seal's creates: the segment, then each of up to five sidecars in sequence,
as every fold and compaction seals. Up to `IN_FLIGHT` (4) segments proceed at once. Requests
scale with segments, so with bytes, never with rows, and never with elapsed time: a refused
seal ends the copy. One commit finishes it. Memory: at most four source segments' bytes and
rows at once. HEAD grows by one record per reindex in flight or retained. Nothing here is on
a user-facing path.

## Risks
- A process older than M168 refuses a HEAD carrying the section, or an outcome with the new
  failure code (`CorruptHead`): upgrade every node before offering reindexes, as for every
  section since M22.
- A reindex holds the source's **folded** rows at `source_epoch`, as an export does; writes
  after it go to the source alone. Moving clients to `dest` is the caller's.
- A rebuild costs CPU proportional to the rows: a large index's sync holds a worker for that
  long. It is cancellable, and pause stops it at the next sync.

## Tasks
- M168.1: engine options, HEAD section and failure code, rebuild in the mirror, refusals at
  create, repeated-POST match.
- M168.2: server body, refusals, support matrix, end-to-end test.
