# M9g — Ranking composition: weighted fusion, score sums, several text queries, multi-query

**Serves:** the `rank_by` composition rows and multi-query of
[`turbopuffer-api-parity.md`](../../research/11-design/turbopuffer-api-parity.md), and D-27's
"weighted fusion is the escape hatch". Seventh of M9.
⚠️ Revised at spec review (block): a `sum` leg is **exhaustive** (a truncated leg drops a row's
contribution, and the answer then depends on how the index is cut into segments); a dense leg
is **refused** under `sum`/`max` (its scores can be negative, so "a missing leg adds nothing"
would rank a retrieved row below an unretrieved one). **Out of scope, stated:** turbopuffer's
`rank_by` syntax and BM25 over *different* fields -- an index has one text field; this is the
same composition over several queries of that field, in pstore's own `fusion` shape.
⚠️ **Split after round 2** (block on `sum`'s mechanics), per the review skill: **M9g.1** is
weighted RRF and its `k`, text arrays and multi-query (criteria 1, 3, 4, 5), none of which
round 2 faulted; **M9g.2** is `sum` and `max` (criterion 2), amended with round 2's findings
below, and reviewed again as its own task. Until M9g.2 lands, `sum` and `max` are refused as
unknown kinds.

## Delta

**Several text queries.** `text` may be a string, as today, or an array of 1 to 15 strings.
Each string is its own BM25 leg over the index's text field. With a `vector`, that allows at
most 16 legs.

**Fusion.** `fusion` chooses how the legs combine. The legs are in request order: the dense leg
first, if there is one, then each text query.
- `{"rrf": {"k": 60, "weights": [..]}}`: each leg contributes `w_i / (k + rank)` for a row it
  ranked. This is the default, with `k = 60` and every weight 1, which is today's behaviour
  exactly.
- `{"sum": {"weights": [..]}}`: `Σ w_i · score_i`, summed in leg order in `f32`, over the legs
  that scored the row. A leg that did not score it contributes nothing -- BM25 is never
  negative, so that is its score. **Every leg is exhaustive** (as under a filter, M9b): each
  returns every row it matches, so no row loses a leg's contribution to a cut. Bytes: the
  postings of the query's terms. A weight is turbopuffer's `Product`, a constant scaling one
  leg's score.
- `{"max": {"weights": [..]}}`: `max w_i · score_i`. Legs keep their limit: a row's best leg
  ranks it within that leg's top `top_k`, so no cut loses it.
- `sum` and `max` take **text legs only**; a dense leg is refused.
- (M9g.2, round 2) **Neither post-mask cut applies under `sum`**: each leg's union across
  segments is uncut, and only the fused ranking is cut to `top_k` -- copying the filter path
  literally re-cuts each leg to its limit. And **the legs are fused before the shadow round**:
  the top `top_k + |shadow|` fused candidates are resolved, shadowed ones dropped, then cut to
  `top_k`, because checking the shadow on every exhaustive candidate would fetch a block per
  matching row. Requests unchanged; bytes are the terms' postings plus those rows.

Rules for the fields:
- `weights` is optional; when present, it must have one finite, non-negative number per leg.
- `k` must be finite and positive.
- Anything else is `400 bad_request`: an unknown kind, a wrong weight count, a negative or
  non-finite weight, `k <= 0`, more than 15 text queries, an empty text array, or a dense leg
  under `sum`/`max`.

`score` in a result is the fused value. `$dist` is still the first dense leg's.

**Multi-query.** `{"queries": [q1, …, qn]}`, with 1 to 16 queries, each a query body as today
(a relevance query or a `rank_by` order). The queries run **concurrently**, and each is
answered exactly as it would be alone -- **each against the HEAD it reads**, so a fold landing
mid-request can give sub-queries different epochs, each reported as a single query reports its
own. A sub-query may carry `as_of`. The response is `{"results": [[rows], …], "meta":
{"epochs": [..], "unfolded_hits": [..], "cost": {..}}}`. There is one `cost`, for the whole
request, because the tenant's spend is not attributable per query when they overlap. (Like
every `cost`, it also counts any other request of the tenant's in flight at the same time.)

`queries` beside any other field, a nested `queries`, zero queries or more than 16 are
`400 bad_request`. One refused sub-query refuses the request: nothing partial is returned.

**Engine and query crate.** `Fusion` gains `WeightedRrf`, `Sum` and `Max`, each carrying at
most 16 weights, so it stays `Copy`. `fuse` applies them; weights make it depend on leg order,
which its doc must say. `run` makes every leg exhaustive under `Sum`, as it does under a filter.

**Does not change:** any request without `fusion`, a text array or `queries`; the format; the
depth of any query.

## Acceptance criteria

1. With weights `[1, 0]` on a vector and a text leg, where the vector leg returns at least
   `top_k` rows, the RRF order equals the vector leg's own order. At `k = 1`, a 5:1 weight puts
   the heavier leg's own first hit first, either way round (code review: at `k = 60` RRF is
   too flat for the weights to decide this fixture). One leg's first place with weight `w` scores
   exactly `w / (k + 1)` in `f32`, for `k = 60` and `k = 10`.
2. With two text queries, `sum` and `max` order and score every row as the leg-order `Σ` and
   the `max` of the single-leg scores, within `1e-5` relative. Each single-leg score is taken
   from the same query run alone with `{"sum": {}}` and a `top_k` of at least every matching
   row, which returns the raw BM25. Weights scale each leg's contribution. And the truncation
   case: `top_k = 1`, where the row with the best sum is neither leg's own first, returns that
   row.
3. A multi-query of a vector query, a two-text `sum` query and a `rank_by` order returns, per
   query, exactly what each returns alone. `cost.blob_lists` is 0.
4. Each refusal above is `400`, including a sub-query's own refusal. A default query's response
   is byte-for-byte unchanged (existing tests pass untouched).
5. `./scripts/gates.sh` passes; `./scripts/mutants.sh` over the diff misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | `fusion` ignored | a weight unapplied; `k` off by one in the rank |
| 2 | `text` must be a string | `sum` as `max`; a weight applied to the wrong leg; a `sum` leg not exhaustive |
| 3 | `queries` ignored | results out of request order; a sub-query sharing another's legs |
| 4 | accepted today | a bound unchecked |

## RA budget

Unchanged for a single query, except `sum`: its legs are exhaustive, so its bytes are every
posting of its terms, at the same depth. A multi-query of `n` issues at most `n` queries'
requests, at the depth of the deepest one, because they run concurrently.

## Risks

- A `sum` query reads all its terms' postings; a common term makes that the index's size.
- Sub-queries of one request may be answered at different epochs.

## Tasks

- **M9g.1** — `rrf` with `k` and weights, text arrays, multi-query; criteria 1, 3, 4, 5.
- **M9g.2** — `sum` and `max`, with the round-2 amendments; criterion 2, and a criterion that a
  `sum` query's reads with a non-empty shadow do not grow with the rows it matches.
