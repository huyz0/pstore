# M142: outer RRF over a multi-query

## Serves
Workstream F of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Outer RRF
has integer `rank_constant`, positive weights in query order, correct deduplication and
deterministic ties ... Post-fusion limit/offset are applied at the correct stage. Compare
the complete result with a simple reference implementation." Closes the "multi-query
`rerank_by`" and "multi-query `limit`, `offset`" rows of [the
comparison](../../comparisons/turbopuffer-2026-10-25.md) for RRF; model reranking stays
J's. Extends [M9g](../M9g/SPEC.md)'s multi-query. D-34's depth budget is unchanged.

## Delta
Today `{"queries": [...]}` is the whole body: any other root key is refused, and the
answer is each query's results, in request order.

After, the root may also carry:
- `rerank_by`: `["RRF"]` or `["RRF", {"rank_constant": n, "weights": [w, ...]}]`.
  `rank_constant` is a JSON integer of at least 1 (`60.0` is not one), default 60.
  `weights` has one number per query, each greater than 0 and at most 1,000,000, in query
  order; default every weight 1. The parameters object has no other key.
- `limit`: a JSON integer of at least 0, or `{"total": n}`, which means the same, with
  `total` required and no other key; default every fused row. `offset`: an integer of at least 0, default 0. Both only beside
  `rerank_by`.

With `rerank_by`, each query runs exactly as it does today, its own `top_k`, filters and
fusion included, and its rows are its ranking. A document's fused score is the sum, over
the queries whose results hold it, of `weight / (rank_constant + rank)`, the rank counted
from 1 in that query's answer as returned (a `rank_by` query's own `offset` rows are not
in it; its rank is its position, as it has no score). The arithmetic is `f64`: each
contribution is computed in `f64`, and a document's contributions are added in query
order. One row per document id, whatever number of queries returned it. Rows are ordered
by the `f64` fused score, descending, and equal `f64` scores by id, ascending (bytewise).
Then `offset` rows are skipped and `limit` kept. The score is cast to `f32` only in the
answer.

The queries may be answered at different epochs, as today when they do not share a level
or one reads `as_of` (M131 pins only the others); fusion takes each answer as it is.

The answer keeps the multi-query shape: `results` holds one list, the fused one, and
`meta` is unchanged, one entry per query. A fused row carries `id`, `score` (the fused
score), and `attributes` from the first query, in request order, whose results hold it
with attributes, an empty object counting as attributes (so a row two queries return at
different epochs carries the first's);
no `$dist`, and no `$highlight`.

Refused with `bad_request`, before any query runs: `rerank_by` that is not one of the two
forms, including a model reranker's object (J's, not offered); `rank_constant` that is
not a JSON integer of at least 1; `weights` whose length is not the number of queries,
or with a weight that is not a number in (0, 1,000,000]; an unknown key in the
parameters; `limit` or `offset` without `rerank_by`; `limit` or `offset` that is not a
JSON integer of at least 0; a `limit` object without `total` or with another key; any
other root key, as today.

How: the server parses the root into the sub-queries and an optional outer fusion, runs
the queries as today (M131 pins them to one snapshot when they share a level), then fuses
their result rows in memory with a pure function, `fuse_outer`, in `pstore-server`.

Unchanged: every sub-query's own refusals and answer; a multi-query without `rerank_by`;
the per-query `meta`; the 16-query bound.

## Acceptance criteria
1. `fuse_outer`, over pseudo-random rankings (1 to 16 lists, overlapping ids, many equal
   scores) with pseudo-random weights, constants, offsets and limits, equals a reference
   implementation written separately in the test: a map from id to summed score, sorted,
   sliced.
2. Server, three queries over one corpus with overlapping results -- a dense query, a text
   query, and a `rank_by` query with its own `offset`, two of them asking for different
   attributes -- the fused `results[0]`
   equals the reference applied to the three queries' own answers (each run alone), for
   the default parameters, for given `rank_constant` and `weights`, and with `limit` and
   `offset`; `{"total": n}` answers as `n` does. A document two queries return appears
   once. Fused rows carry the first asking query's attributes.
3. Deterministic ties: two documents with equal `f64` fused scores are ordered by id,
   ascending, whatever the queries' order in the request. The test's ties are exact by
   construction (each tied score one contribution, or two added in either order).
4. Every refusal in the Delta answers `bad_request` and makes no blob request.
5. A multi-query without `rerank_by` answers exactly as before: the existing
   `composition::` and `snapshot::` multi-query tests pass unedited.
6. Depth: a fused multi-query of a dense, a text and an ordered query is no deeper than
   the same multi-query without `rerank_by`, and makes the same blob requests, measured by
   `pstore_testkit::depth::DepthCounting`.

## Test plan
Unit tests beside `fuse_outer` in `crates/pstore-server/src/`; server tests in a new
`crates/pstore-server/tests/it/rerank.rs`. Red before: 1 fails to compile without
`fuse_outer`; 2, 3 and 6 fail (the parent refuses `rerank_by` as a root key); 4's cases
pass on the parent, which refuses every root key, so their red is the mutations named
below; the refusals no mutation names (the model reranker's object, a non-integer, an
unknown key, `offset` without `rerank_by`, a `limit` object without `total`) are guards,
and VERIFIED.md says so. 5 is a guard; no existing test asserts the old refusal's text. Mutations, each to be killed:
- rank counted from 0 (1, 2); weights taken in reverse query order (1, 2);
- the sum replaced by the maximum (1, 2); duplicates kept (1, 2);
- ties broken by id descending, or by first appearance (1, 3);
- `offset` applied after `limit` (1, 2);
- a weight of 0 accepted; a weight over 1,000,000 accepted; a `rank_constant` of 0
  accepted; a weights count mismatch accepted; `limit` without `rerank_by` accepted (4).

## RA budget
Unchanged: no request is added; the fusion reads only the queries' answers. Its memory is
a map over the rows those answers already hold, so it at most doubles what the request
holds today; no new bound is claimed, as a dense or text query's `top_k` has none here
(M128's ingress limits are workstream A's).

## Risks
- A query's own ranking may come from peers (M54) or carry unfolded rows; the fusion sees
  only its answer, so this changes nothing there.
- `top_k` per query is the fusion's candidate window: a document no query ranks within its
  `top_k` cannot be fused. That is the vendor's "each leg's requested global prefix".

## Tasks
- M142.1: the root's parsing, `fuse_outer`, the answer, docs.

## Review
Spec review round 1 (block on one major, amended): the arithmetic is `f64` in query order,
ties compare the `f64`, and the cast to `f32` is at output; weights bounded at 1,000,000.
Minors taken: a sub-query's own `offset`; legs at different epochs; integers and the
`limit` object stated; unnamed refusals declared guards; the memory claim corrected; AC2's
legs named.
Code review round 1 (pass with minors): the depth test measures each run on a cold server
and asserts it measured something; an empty attributes object counts as attributes, now
stated.
