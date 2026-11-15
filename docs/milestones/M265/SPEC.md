# M265: `compute_attributes`: per-row expression scores and vector distances

## Serves
The [turbopuffer gap list](../../comparisons/turbopuffer-gaps-2026-11-15.md),
"`compute_attributes`": the vendor evaluates up to 256 named expressions on each returned
row (`VectorDist`, any BM25 or full-text clause, `Highlight`), never changing the ranking. It
is the documented way to feed a reranker per-clause scores. Shapes are those of the pinned
SDK, `turbopuffer` 3.1.0 (`types/custom.py`, `Expr`). Workstream E of
[the delivery plan](../../comparisons/turbopuffer-next-plan.md). No D- or OQ- id covers it.
`Highlight` within `compute_attributes`, with `rank_fragments_by`, is M266's.

## Delta
1. **`compute_attributes`** on a relevance query (`vector`, `text`, or both): an object of
   1 to 256 names, each to one expression:
   - **`[field, "VectorDist", [f32, ...]]`**: the distance from the row's `field` vector
     (`"vector"`, or a named field, M190) to the given vector, under the index's metric,
     as `$dist` reports a distance. A named field holding several vectors (M191) answers
     its nearest. The width must be the field's.
   - **any `rank_by` scoring expression** (M182, M203): `BM25` clauses over the primary or a
     further text field, `Sum`, `Max`, `Product`, filters, `Attribute`, `Saturate`,
     `Decay`, `Dist`, with the limits `Expr::check` sets, **except** its root rule: a
     computed value ranks nothing, so it may be signed (`Expr::check_limits`).
2. **Reported per row** under `"$computed": {name: value}`: a number, or `null` where the
   value is absent (a row without the vector field; a root `Attribute`/`Dist` with no
   value, or a named field no row holds). An expression's value is **the score `rank_by`
   with the same expression would give that row**, the same `f64` narrowed to `f32`, but
   not clamped away from 0: a row a BM25 clause does not hit reports `0`.
3. **Never changes** the ranking, the page, `score`, `$dist` or `include_attributes`; a
   `VectorDist` field's vectors are fetched but spelled only where `include_vectors` asks.
   Under a multi-query's outer fusion (M142) a fused row carries no `$computed`, as it
   carries no `$highlight`.
4. **The same snapshot, no extra round.** BM25 statistics (each field's row count, length
   and terms' document frequencies, over segments and unfolded rows) are read from the
   query's snapshot, pinned as a faceted query pins (M131), concurrently with the query's
   own rounds: the request's round-trip depth is the query's without `compute_attributes`.
   A `VectorDist` field's vectors ride the row fetch, as projection does (M179).
5. **Refused `400` before any blob request:** on an order, an aggregation, an expression
   `rank_by` query or `as_of`; none or more than 256 names; an empty name or one starting
   with `$`; a malformed expression (`rank_by`'s own messages); `VectorDist` on the sparse
   field, or with an empty or non-numeric vector. **Refused `400` from the snapshot**,
   which holds the index's schema: a `BM25` over a field that is not a text field (as
   `rank_by` refuses it), and a `VectorDist` whose width differs from a returned row's.

**No setting.** Nothing changes for a query without `compute_attributes`; with it, the cost
is what the caller asked for. That is the settings rule's opt-in case.

**Not changed:** every other query, the fetch round's rows, the wire of a query without it.

## Acceptance criteria
1. **BM25 equals `rank_by`.** `computed_bm25_equals_rank_by`, in
   `crates/pstore-server/tests/it/computed.rs`: over a folded segment, a second segment and
   unfolded rows, a vector query's and a text query's page with
   `compute_attributes: {"s": E}` (E a `Sum` of two `BM25` clauses, a `Product` and a
   filter) reports, for each row, `$computed.s` exactly equal to the `score` a
   `rank_by: E` query gives the same row (rows it does not return: `0`); a root
   `Attribute` reports the row's value, and `null` where it has none.
2. **Ranking unchanged.** `computed_attributes_never_change_the_page`: the same query with
   and without `compute_attributes` returns the same ids, scores and distances in order.
3. **VectorDist.** `computed_vector_dist`, under each of the three metrics: `VectorDist`
   from the query vector equals each row's exact `$dist`, and from another vector the
   metric's distance computed in the test from the written vectors, both within `1e-5`
   relative; on a named field (M190) likewise; a row without the field `null`; a vector
   of another width refused.
4. **Depth.** `computed_attributes_add_no_round`: with `DepthCounting`, a text query's and a
   vector query's depth with `compute_attributes` (a BM25 clause and a `VectorDist`) equals
   their depth without.
5. **Refusals.** `computed_attributes_refusals`: each refusal in Delta 5, `400`, naming
   why, with no blob request.
6. **Gates:** fmt, clippy, the workspace tests, `scripts/gen-clients.py --check` (the
   OpenAPI query and result row gain the fields), and the named mutations observed red.

## Test plan
AC1 to AC5 are seen red against the parent, which refuses `compute_attributes` as an
unknown field.

| AC | Mutation |
|---|---|
| 1 | the computed statistics leaving out the unfolded rows; a root with no value reported as `0`; values given to the wrong rows |
| 3 | dot product measured as L2; the width check dropped |
| 4 | statistics read after the query instead of beside it |
| 5 | the refusal of a non-relevance query dropped |

## RA budget
Depth unchanged (AC4). Requests: with a `BM25` clause, each segment's footer, delete
vector and dictionary, in the round the query opens its segments. Behind the read cache the
footer and delete vector are the query's own reads, so only the dictionary is new (a text
leg over the field already reads it).
A `VectorDist` adds the field's vectors to the rows fetched, nothing else.

## Risks
- **Large pages** evaluate every expression on every row: 256 expressions on 1,200 rows
  (`MAX_TOP_K`) is CPU only, bounded by `Expr::check`'s node limit.

## Tasks
- **M265.1** The spec.
- **M265.2** `pstore-query`'s evaluator (statistics and per-document scoring, shared with
  `rank`), the engine's statistics read, the server's parse, pinning and rendering, the
  OpenAPI fields, tests, docs.
