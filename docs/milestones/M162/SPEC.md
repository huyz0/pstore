# M162: an index measures its own recall, online, at one snapshot

Spec review round 1 failed with three majors, now fixed. The searches reused the `as_of`
query path, which reads HEAD itself: each would have cost a HEAD read and risked a reap
between them; they now run against the one HEAD the request read. A filter makes every
dense leg exhaustive (M9b), so the filtered approximate search scores every row too: the
budget counts it, and the spec says a filtered recall measures quantization, not probing.
And an index with no default vector field, and a replica, now have their statuses. Its
three minors are folded in: the sampled `query_vector` is the stored vector, the
clustered test fixes its corpus so a miss is certain, and the budget's count is HEAD's.
Round 2 passed with three minors, folded in: the sampled vector is in client space, the
replica's status is a criterion, and task M162.1 names the factored function.

## Serves
Workstream H of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Online
recall samples ANN and exhaustive results at one snapshot, with reproducible
seeds/filters/ground truth and bounded work", and its budget: "Sampling/explain do not
secretly force gate-scale exhaustive scans on ordinary user queries." The comparison
lists `POST /v1/namespaces/{namespace}/_debug/recall` as missing, with only the offline
`scripts/recall.sh` ([comparison](../../comparisons/turbopuffer-2026-10-25.md)), and its
response fields `avg_recall`, `avg_exhaustive_count`, `avg_ann_count` and
`ground_truth[{query_vector, nearest_neighbors}]`.

## Delta
Today recall is measured only offline, by `scripts/recall.sh` over a synthetic corpus. A
deployment cannot ask how well its own index's approximate search agrees with an exact
one. The pieces exist: a query answers at a past epoch (`as_of`, M10), a dense leg can
score every row at full precision (`exact`, M9d), and a query can return its hits'
vectors (M64).

After:
- **`POST /v1/indexes/{index}/recall`** (scope `read`), body
  `{"num", "top_k", "filters", "seed", "include_ground_truth"}`, every field optional,
  unknown fields refused:
  - `num`: queries to sample, 1 to 25, default 10.
  - `top_k`: results compared per query, 1 to 100, default 10.
  - `filters`: as a query's (M9b), applied to both searches of every sampled query.
  - `seed`: an unsigned 64-bit integer, default 0.
  - `include_ground_truth`: default false.
- **One snapshot, one HEAD read.** The request reads HEAD once, and every search runs
  against that HEAD's segments and delete vectors, as an `as_of` query at its epoch
  would, without reading HEAD again: rows no fold has committed are in no snapshot and
  are not measured, and no reap between the searches can move the snapshot. The
  response's `epoch` names it.
- **Sampling, reproducible.** A probe vector is drawn from `seed` by a fixed generator
  (each coordinate uniform in [-1, 1), over the index's default vector field and its
  dimensions). One approximate search for it, at the snapshot, without `filters`, returns
  up to `num` documents with their vectors; each of those vectors is a sampled query.
  The same `seed` over the same snapshot samples the same queries. A sampled query is a
  document's vector in client space as a query returns it (under `cosine_distance` the
  written vector normalized), and is what `ground_truth`'s `query_vector` reports. Fewer than `num`
  documents sample fewer queries; none answers with `num` 0.
- **Measuring.** For each sampled query, at the snapshot and with `filters`: the
  approximate search a default query runs (`top_k` results), and the exhaustive one
  (`exact`, every row scored at full precision, `top_k` results). Its recall is the share
  of the exhaustive ids the approximate search also returned; a query whose exhaustive
  search returns nothing has no recall and is left out of the average. The response is
  `{"avg_recall", "avg_ann_count", "avg_exhaustive_count", "num", "epoch", "cost"}`, with
  `avg_recall` null when no query has a recall, and with `include_ground_truth`,
  `"ground_truth": [{"query_vector", "nearest_neighbors": [{"id", "dist"}]}]`, the
  exhaustive results in rank order with the distance a query reports.
- **With `filters`, recall measures quantization only.** A filter makes every dense leg
  probe every list (M9b), so the filtered approximate search scores every row's codes
  and can differ from the exhaustive one only by its compressed scoring, never by a
  missed list. The doc says so.
- **Bounded, and paid for explicitly.** The exhaustive searches read every vector of the
  index, which is what the request is for and is reported in `cost`. A search that
  scores every row counts the snapshot's rows: each exhaustive search does, and with
  `filters` each approximate one too. The rows are the sum of HEAD's `SegmentRef.rows`
  for the index (deleted and superseded rows included, so the count over-states, never
  under-states). The request is refused from HEAD alone, before any segment read,
  `400 recall_too_large`, when (`num`, or `2 × num` with `filters`) times those rows
  exceeds `RECALL_ROW_BUDGET = 2,000,000`, so a caller asks for fewer queries rather than
  being charged for a scan it did not size. No ordinary query changes.
- **What has no recall to measure.** An index whose default vector field has no
  dimensions (text-only, or only named vector fields) is `400 recall_needs_vectors`
  before any segment read. A replica is `409 replica_no_history`, as its `as_of` query
  is.
- **Depth.** The HEAD read, the probe search, and one round of all `2 × num` searches at
  once (fan-out within a round is free): each search as deep as the query it is, with the
  exhaustive one outside the D-34 budget as `exact` already is. No LIST, no write.
- **Engine.** `Engine::recall(index, &RecallRequest) -> Result<Option<Recall>,
  EngineError>`, `None` for an index the snapshot does not hold. Its searches go through
  one function that takes the HEAD already read, factored out of `query_as_of_filtered`
  (which then calls it), with the vectors option `query_rows_split` already has.

Not changed: every existing route, the query path, HEAD, and the stored formats.

## Acceptance criteria
1. Over a clustered index (a fixed corpus folded with an exact-scan threshold and list
   size small enough that its segments carry many more lists than the default probe
   width, so the default search misses rows): `avg_recall`, `avg_ann_count` and
   `avg_exhaustive_count` equal the values the test computes from the same sampled vectors
   by running the default query and an `exact` query itself, and `avg_recall` is below 1.
   Over an index small enough to be scanned exactly, `avg_recall` is 1.
2. The same `seed` over the same snapshot returns the same ground truth; another seed
   samples other queries; each sampled `query_vector` is a document's vector in client
   space (normalized under `cosine_distance`).
3. With `filters`, every ground-truth neighbour satisfies the filter, the counts equal
   those the test computes from filtered queries of its own, and a filter no row
   satisfies gives `avg_recall` null and both counts 0.
4. Rows written and not folded are not measured, and a write between two recall requests
   that is then folded changes `epoch`.
5. `num` 0 or 26, `top_k` 0 or 101, a negative or fractional `seed`, and an unknown field
   are `400`; the row budget exceeded (and exceeded only because of `filters`' doubling)
   is `400 recall_too_large` with no read past HEAD; an index with no default vector
   dimensions is `400 recall_needs_vectors`; a replica is `409 replica_no_history`; an
   absent index is `404`.
6. Cost: one HEAD read whatever `num` is, no LIST and no write; the other reads are the
   probe search's and the `2 × num` searches', which the response's `cost` reports.
7. Gates: clippy, the workspace suite, coverage 95/95, mutation of the changed code.

## Test plan
- Engine: `recall_matches_its_own_queries` (1), `recall_is_reproducible` (2),
  `recall_with_filters` (3), `recall_is_one_snapshot` (4).
- Server: `recall_route` (1's exact case, 3, 5 with the replica, 6).

## RA budget
Per request: one HEAD read, one probe search, then `2 × num` searches in one round, none
reading HEAD again. At most 51 searches; every search that scores every row (the
exhaustive ones, and with `filters` the approximate ones) is counted against
`RECALL_ROW_BUDGET` row-scores per request. Requests scale with segments and bytes, never
with a request per document, and nothing runs unless a caller asks for it.

## Risks
- Sampled queries are documents near one random probe, not a uniform sample of the
  index: a deliberately cheap, reproducible sample. The response says what was measured
  (`num`, `epoch`), and the doc says how it was sampled.
- Recall over the folded snapshot can differ from what a live query sees with unfolded
  rows fused; the response's `epoch` says which index was measured.

## Tasks
- M162.1: `Engine::recall` (probe, sampling, both searches, the averages), and the search
  function taking the HEAD already read, factored out of `query_as_of_filtered`, with
  `query_rows_split`'s vectors flag.
- M162.2: the route, its validation and budget, and docs (api-migrations, DELIVERY,
  comparison).
