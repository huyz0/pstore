# M165: a query explains the plan it ran

Spec review round 1 failed with one blocking finding and five majors, all fixed here.
- Blocking: a leg runs on a spawned task (M59), out of a task-local's reach. Its strategy
  is now part of what it returns.
- `probes` is the lists actually probed, summed, because widening scales `p` per segment.
- `kept` is the hits left after the cut `candidates` makes, so it is at most `limit` plus
  the shadow.
- `shadowed` is the widening count, because no segment knows which ids it holds. The
  fresh segment is excluded by its ordinal.
- Each attempt and each sub-query collects afresh, and a multi-query refuses `explain`.
- `remote_segments` counts only the shares a peer answered.

Minors folded in: legs are reported in request order, and "approximate" is whatever the
search used, centroids or `fde` clusters.
Round 2 passed with one minor, which needs no change. A segment with a centroid table
but no quantized section returns nothing early. Its strategy is taken from what `search`
did, as the spec says.

## Serves
Workstream H of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Explain
shows actual planning choices, candidate strategy and estimated/observed costs without
leaking another tenant", under its budget: "Sampling/explain do not secretly force
gate-scale exhaustive scans on ordinary user queries."

## Delta
Today a ranked query reports its `cost` (requests and bytes) and its `epoch`, and nothing
about how it was planned. The following are all decided per segment and per leg inside
`pstore-query`, and never leave it:
- whether its dense leg probed lists or scanned exactly;
- whether a filter or `sum` fusion made a leg exhaustive (M9b, M9g.2);
- how far deleted and superseded rows widened it (M9c);
- how many of its segments another server scored (M54).

After:
- **`"explain": true`** on a ranked query (`POST /v1/indexes/{index}/query`) adds
  `meta.explain`. When it is absent or `false`, the response is byte for byte what it was.
  The query answers `400` when `explain` is:
  - not a boolean;
  - set on a `rank_by` order or an aggregation, which run no leg;
  - set on a query inside a multi-query.
- **`meta.explain`** is `{"segments", "shadow", "remote_segments", "filter", "fusion",
  "legs"}`:
  - `segments`: the folded segments the query targeted.
  - `shadow`: the unfolded ids each folded segment was widened by (M9c), 0 when this
    process has none.
  - `remote_segments`: the segments whose share a peer answered (M54). A share that failed
    and ran here instead counts as local.
  - `filter`: whether a filter applied.
  - `fusion`: `rrf`, `weighted_rrf`, `sum` or `max`.
  - `legs`: one entry per leg, in request order, each
    `{"kind", "field", "limit", "mode", "requested", "candidates", "kept"}`, over the local
    folded segments. The segment that holds this process's unfolded rows is excluded by
    its ordinal, which the engine passes.
- **Each leg's figures:**
  - `kind`: `dense`, `sparse`, `multi`, `multi_fde` or `text`.
  - `field`: the leg's field.
  - `limit`: the rows the leg contributes.
  - `mode`:
    - `limited`: each segment was asked for `limit`.
    - `widened`: some segment was asked for more, to survive its deleted and shadowed rows
      (M9c).
    - `exhaustive`: every index row of every segment. An exhaustive leg also carries
      `"exhaustive_because"`, which is `filter` or `sum`.
  - `requested`: the rows the leg asked of each segment, summed.
  - `candidates`: the hits those segments returned.
  - `kept`: the hits left after the filter's mask, the delete vectors and the per-segment
    cut to `limit` plus the shadow (`room` in `candidates`), the input to the id-level
    shadow check and fusion.
  - A `dense` leg also carries `"approximate_segments"`, `"exact_segments"` and
    `"probes"`:
    - approximate segments are those its search answered from clusters, either a centroid
      table or a named field's `fde` sidecar (M83);
    - exact segments are those it scanned at full precision;
    - `probes` is the lists those approximate searches probed, summed, each the smaller
      of the `p` asked and the segment's lists.
- **The run's own decisions.**
  - A leg's task returns the strategy its search took beside its hits.
  - `candidates` records each (segment, leg) pair from the leg it actually ran, after
    widening or the exhaustive rewrite, and from that returned strategy.
  - The collector is scoped to the request's task, which `run` and `candidates` run on.
    The legs' and openings' own spawned tasks only return values to it.
  - Each attempt `query_split_inner` or `pinned` makes starts it afresh, so a failed
    attempt leaves nothing.
- **Free.** Explain issues no blob request and reads no byte: `meta.cost` and the
  results equal those of the same query without it. It reports counts and the request's
  own field names only, never keys, segment names, peers or another tenant's figures.

Not changed: every other route and field, the query plan itself, HEAD and the formats.

## Acceptance criteria
1. Over a clustered index with no unfolded rows, whose segments each carry at least 8
   lists, a default dense query reports:
   - `mode` `limited`;
   - `approximate_segments` equal to the segments, and `exact_segments` 0;
   - `probes` 8 times the segments;
   - `requested` `limit` times the segments, `kept` at most `requested`, and `shadow` 0.

   With `exact: true`, every segment is exact and `probes` is 0.
2. With a filter, every leg is `exhaustive` because of `filter`. `requested` is the index
   rows of the segments, and `kept` is the admitted, undeleted hits up to `limit` per
   segment. Under `sum` fusion every leg is `exhaustive` because of `sum`.
3. After deletes are folded into a segment's delete vector, the dense leg is `widened`.
   `requested` is `limit` times the segments plus the deleted rows. With `n` unfolded
   overwrites in this process, `shadow` is `n`, and the fresh segment is not counted in
   `segments` or `legs`.
4. A text leg reports `kind` `text`, its field, and `candidates` equal to the BM25 matches
   up to each segment's limit. Two text legs and a dense leg are reported in request order.
5. Split across two servers:
   - `remote_segments` is the segments the other server answered, and `legs` counts only
     the local ones;
   - with the other server failing, its share runs here, `remote_segments` is 0, and
     `legs` counts every segment.
6. With and without explain:
   - the results and `meta.cost` are equal;
   - without it, the response has no `explain` key.
7. The query answers `400` for:
   - `explain` as a string;
   - `explain` with `rank_by` or `aggregate_by`;
   - `explain` inside a multi-query.
8. A query whose first attempt fails partway, and whose retry succeeds, reports only the
   retry's figures.
9. Gates: clippy, the workspace suite, coverage 95/95, and mutation of the changed code.

## Test plan
- Query: `explain_records_each_segment_leg`, at the `pstore-query` level over segments it
  builds. Covers criteria 1, 2, 3 (deletes) and 4.
- Engine: `explain_counts_shadow_and_excludes_the_fresh_segment` (3) and
  `explain_keeps_only_the_last_attempt` (8).
- Server:
  - `explain_reports_the_plan` (criteria 1, 6, 7);
  - `explain_counts_remote_segments` (criterion 5).
- Mutations:
  - a mode or a reason swapped;
  - widening not recorded;
  - the dense strategy taken from the request rather than the search;
  - `probes` not capped at the lists;
  - a failed share counted as remote;
  - `kept` taken before the mask;
  - the fresh ordinal counted;
  - an attempt not reset;
  - explain added to a response that did not ask for it.

## RA budget
Unchanged: explain adds no request and no round trip.

## Risks
- A figure recorded beside the decision rather than from it could drift from the run as
  the planner changes. The tests compare exact counts, so a drift fails.
- The collector is a task-local, as `PINNED` (M131) is. Work moved to a newly spawned
  task that records directly would go unreported. Only `candidates`, `run` and the
  engine's attempt loops record, and missing figures fail the count tests.

## Tasks
- M165.1:
  - the collector in `pstore-query`;
  - the strategy returned by a leg;
  - recording in `candidates` and in `run`'s shares.
- M165.2:
  - the engine's attempt reset and fresh ordinal;
  - the request flag, `meta.explain` and the refusals;
  - docs: api-migrations, DELIVERY and the comparison.
