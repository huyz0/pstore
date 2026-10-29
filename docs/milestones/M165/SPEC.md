# M165: a query explains the plan it ran

## Serves
Workstream H of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Explain
shows actual planning choices, candidate strategy and estimated/observed costs without
leaking another tenant", under its budget: "Sampling/explain do not secretly force
gate-scale exhaustive scans on ordinary user queries."

## Delta
Today a ranked query reports its `cost` (requests and bytes) and its `epoch`, and nothing
about how it was planned. Whether its dense leg probed lists or scanned exactly, whether
a filter or `sum` fusion made a leg exhaustive (M9b, M9g.2), how far deleted and
superseded rows widened it (M9c), and how many of its segments another server scored
(M54) are decided per segment and per leg inside `pstore-query` and never leave it.

After:
- **`"explain": true`** on a ranked query (`POST /v1/indexes/{index}/query`) adds
  `meta.explain`. Absent, or `false`, the response is byte for byte what it was. Anything
  but a boolean is `400`. On a `rank_by` order or an aggregation, which run no leg, it is
  `400`.
- **`meta.explain`** is `{"segments", "shadowed", "remote_segments", "filter", "fusion",
  "legs"}`:
  - `segments`: the folded segments the query targeted. `shadowed`: those with a newer
    unfolded operation on some id (M9c). `remote_segments`: those whose legs another
    server scored (M54); their figures are not in `legs`.
  - `filter`: whether a filter applied. `fusion`: `rrf`, `weighted_rrf`, `sum` or `max`.
  - `legs`: one entry per leg, in the order the query runs them, each
    `{"kind", "field", "limit", "mode", "requested", "candidates", "kept"}`:
    - `kind` is `dense`, `sparse`, `multi`, `multi_fde` or `text`. `field` is the leg's
      field, and `limit` is the rows the leg contributes.
    - `mode` is `limited` (each segment asked for `limit`), `widened` (some segment asked
      for more, to survive its deleted and shadowed rows, M9c) or `exhaustive` (every row
      of every segment). An exhaustive leg also carries `"exhaustive_because"`: `filter` or
      `sum`.
    - `requested`: the rows the leg asked of every local segment, summed. `candidates`:
      the hits those segments returned. `kept`: the hits left after the filter's mask,
      the delete vectors and the shadow cut, which go on to fusion.
    - A `dense` leg also carries `"approximate_segments"` and `"exact_segments"`, the local
      segments it probed by clusters or scanned exactly, and `"probes"`, the lists asked of
      each approximate one (absent when none was approximate).
- **The run's own decisions.** Each figure is recorded where the run makes its choice:
  the leg each segment ran, after widening or the exhaustive rewrite, and the dense
  strategy the search took. Explain cannot report a plan the query did not run. The rows
  of this process's unfolded writes are not segments, and are not in `legs`.
- **Free.** Explain issues no blob request and reads no byte: `meta.cost` and the
  results equal those of the same query without it. It reports counts and the request's
  own field names only, never keys, segment names, peers or another tenant's figures.

Not changed: every other route and field, the query plan itself, HEAD and the formats.

## Acceptance criteria
1. Over a clustered index, a default dense query reports `mode` `limited`,
   `approximate_segments` equal to the segments with a centroid table, `exact_segments`
   the rest, `probes` 8, `requested` `limit` times the segments, and `kept` at most
   `requested`. With `exact: true` every segment is exact and `probes` is absent.
2. With a filter, every leg is `exhaustive` because `filter`, `requested` the index rows
   of the segments, and `kept` the admitted hits; under `sum` fusion every leg is
   `exhaustive` because `sum`.
3. After deletes folded into a segment's delete vector, the dense leg is `widened` and
   `requested` is `limit` times the segments plus the deleted rows. A query with an
   unfolded overwrite reports that segment in `shadowed`.
4. A text leg reports `kind` `text`, its field, and `candidates` the segments' BM25
   matches up to their limit. Several legs are reported in run order.
5. Split across two servers, `remote_segments` is the segments the other server scored,
   and `legs` counts only the local ones.
6. The results and `meta.cost` are equal with and without explain, and without it the
   response has no `explain` key. `explain` as a string is `400`, and with `rank_by` or
   `aggregate_by` it is `400`.
7. Gates: clippy, the workspace suite, coverage 95/95, mutation of the changed code.

## Test plan
- Query: `explain_records_each_segment_leg` (1-3, at the `pstore-query` level, against
  the segments it builds).
- Server: `explain_reports_the_plan` (1, 2, 3, 4, 6), `explain_counts_remote_segments` (5).
- Mutations: a mode or reason swapped, widening not recorded, the dense strategy reported
  from the request rather than the segment, a remote segment counted locally, `kept`
  taken before the mask, explain added to an unasked response.

## RA budget
Unchanged: explain adds no request and no round trip.

## Risks
- A figure recorded beside the decision rather than from it could drift from the run as
  the planner changes: recording happens in the function that makes the choice.
- The collection rides a task-local, as `PINNED` (M131) does: a leg moved to a task
  spawned outside it would go unreported. The tests compare counts, so a missing leg fails.

## Tasks
- M165.1: the collector in `pstore-query`, recorded in `candidates` and the dense leg.
- M165.2: the request flag, `meta.explain`, the refusals, and docs (api-migrations,
  DELIVERY, comparison).
