# M147: an index can be created empty

Spec review round 1 failed with three majors: empty `indexes` lists already exist (an
index whose rows were all deleted and compacted), the create must update this process's
commit and schema caches, and the existence sites were four, one in the server. All three
are folded in below, with the minors. Round 2 passed with four wording minors, also
folded in.

## Serves
Workstream G of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Empty
namespaces survive restart and list correctly ... Deleted names can be reused without old
WAL or replication work reviving a previous incarnation", and the comparison's
`create_namespace` row ("No empty namespace creation"; [comparison](../../comparisons/turbopuffer-2026-10-25.md)).

## Delta
Today an index comes into being only when a fold seals a segment for it (or records a
reject or a quarantine for it); its schema is inferred by that fold. Nothing creates one
empty. An empty `indexes` list already exists, though: compaction of an index whose rows
were all deleted leaves `indexes[index] = []` with its schema. Such an index is listed
and its summary answers 200, but its queries, aggregates and `warm` answer 404.

After, `PUT /v1/indexes/{index}` (scope `write`) with a JSON body
`{"dims": n, "distance_metric": m}` creates the index with no rows:
- `dims` is required, an integer from 1 to the `Api`'s `array_elements` limit (M143,
  16,384 by default): a wider index could never be written. `distance_metric` is optional, with the write
  path's names and default (`dot_product`, M9d); an unknown one is refused as a write's
  is. Any other field is refused. The name follows `valid_name`, as a branch's does.
- The engine reads HEAD and commits one HEAD CAS whose new state has
  `indexes[index] = []` and `schemas[index]` = that width and metric, every other schema
  field at its default (no text field, default full text, no trigrams, no named fields).
  A lost CAS re-reads and retries under `MAX_COMMIT_ATTEMPTS`, as a branch does.
- Refused with `409 index_exists` (a new engine refusal, not `Refused`, which is 400),
  and nothing committed, when the name exists as a drop decides it (a key of `indexes`,
  `schema_rejects` or `quarantine`, or rows this process holds for it), or names an
  external index. A replica's name is refused as a
  branch refuses one (`replica_read_only`). A store without fencing is refused as a
  branch's is. A name dropped within GC's window is **accepted**: the drop recorded its
  old schema in `dropped`, so `as_of` before the drop still reads the old index, and
  this changes nothing a write that recreates the name does not already do.
- On commit the engine calls `record_commit` and `remember_schemas` with the committed
  HEAD, as a fold does: the door then refuses a write of another width at once, and a
  bounded read right after does not serve the HEAD from before the create.
- Answers `200 {"epoch", "cost"}`, as a delete does.

Existence decided from HEAD becomes "a key of `indexes`", not "a non-empty list", at
all four sites: the relevance query (the server's `segments.is_empty() &&
unfolded.is_empty()`, so `Answer` gains an `exists` flag from the HEAD it answers from (the rebuilt one under `as_of`), on every
path that builds one, split and peer paths included), `ordered_once`, the aggregate,
and `warm`. So a created index, **and an index emptied by deletes and compaction**,
answers a query, an aggregate and `warm` with 200 and no rows. A fold keeps an empty
entry. The first write must match the declared schema as a write to a folded index of
that schema must (the door, then the fold's reject pass), naming the metric: a row's
`$metric` is checked against the schema, so a write to a cosine index that omits
`distance_metric` is refused. Its fold seals the first segment under the declared
metric. The created schema has the default analyzer and no trigrams, and a schema is
immutable, so a later write declaring another analyzer or `regex` is refused
`schema_conflict`. Rows another process acknowledged, unfolded, at another width are
quarantined by their fold: the create is linearized at its commit, as a drop is.

Not changed: the HEAD encoding (an empty list already encodes), write and query
semantics for indexes with segments, branch (an empty source is still refused: nothing
folded to branch from), an empty replication source (still `source_not_found`), delete (`DELETE` of an empty index commits its drop and records
its schema in `dropped`, as for any index), replication, and `as_of` (an index with no
segment at an epoch reads as absent at that epoch, as today, so an empty index queried
`as_of` its creation epoch is 404). No text-field, FTS, trigram or named-field
declarations in this slice; no `PATCH` of a schema (still `schema_immutable`).

## Acceptance criteria
1. A created index is listed, its summary is 0 segments, 0 documents, the declared dims
   and metric, and a vector query and a `rank_by` query on it answer 200 with no rows;
   an aggregate with no `group_by` answers 200 with its one empty-keyed group and a
   count of 0, and `warm` reports it exists with 0 segments; all of this again from a second `Api` over the same store (a restart). An
   index emptied by deletes and a compaction of at least two folded segments answers
   the same routes the same (an engine test: no route compacts).
   A query `as_of` the creation epoch is 404.
2. Each refusal (`index_exists` for each existing kind, external, replica, bad name,
   `dims` 0 or past `array_elements`, unknown metric, unknown field; a store without
   fencing is `BackendCannotFence`, 503, asserted on the engine, since no `Api` can be
   built over one) is its status and code, and the store's PUT counter is unchanged.
3. Two creates of one name whose first attempts read the same HEAD: exactly one 200 and
   one 409, and HEAD names the winner's schema.
4. A write of the declared width succeeds and its fold seals one segment under the
   declared metric (query distances are that metric's). In the same `Api` with no read
   between, a write of another width, or omitting the cosine metric, is refused 400 with
   nothing buffered; one declaring a non-default analyzer is `schema_conflict`.
5. A fold of another index keeps the empty entry: still listed, still 200 for a query.
6. `DELETE` of an empty index answers 200; it is then unlisted and 404; the name can be
   created again with another width, and a write of the old width, sent next in the
   same `Api`, is refused.
7. The create issues exactly 1 HEAD GET and 1 conditional PUT on an uncontended store,
   no LIST, asserted by the request-class counters.
8. Gates: clippy, the workspace suite, coverage 95/95, mutation of the changed code.

## Test plan
- `created_index_survives_restart` (1): fails today with 405 for the route. Kills an
  existence check left at `!refs.is_empty()` at any of the four sites (one assertion
  per route) and a create that does not commit the schema.
- `create_refusals_touch_nothing` (2): kills each guard removed; PUT counter catches a
  refusal that commits.
- `concurrent_creates_have_one_winner` (3): the engine's `create_index_with_interference_for_test`
  makes the first attempt lose; kills a retry that does not re-check existence.
- The relevance query, aggregate and `warm` assertions in (1) each kill an existence
  check left at `!refs.is_empty()` at their own site; the emptied-index case kills the
  same at the server's `segments` check.
- `first_write_follows_the_declared_schema` (4): a cosine index's query score is
  `1 - cos`; kills a schema written with the default metric.
- `a_fold_keeps_an_empty_index` (5): kills a fold that drops empty lists.
- `an_empty_index_drops_and_its_name_is_reused` (6).
- `create_costs_one_read_and_one_cas` (7).
- Unit tests in `pstore-server` for body parsing (dims bounds, metric, unknown field).

## RA budget
Create: W 1 (HEAD CAS), Rseq 1 (HEAD GET), List 0; under contention each retry adds one
of each, bounded by `MAX_COMMIT_ATTEMPTS`. Every other route unchanged: the existence
checks read the HEAD they already read.

## Risks
- A site that still reads an empty list as "absent" (or assumes `indexes` lists are
  non-empty, e.g. a `refs[0]`) answers 404 or panics on an empty index. Revealed by
  criterion 1's per-route assertions and a grep for indexing on `indexes` values.
- The door's schema cache is per process; a process whose cached HEAD predates the
  create admits a mismatched write and the fold quarantines it, as today for a folded
  index (M7d). Criteria 4 and 6 pin that this process's own cache is updated.

## Tasks
- M147.1: engine `create_index` with its interference hook; existence sites.
- M147.2: the route, body parsing, refusals and tests; docs (api-migrations, DELIVERY).
