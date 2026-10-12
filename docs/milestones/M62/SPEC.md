# M62 — Ranking expressions: a ranked query's window rescored by its attributes

**Serves:** backlog row 66, from the project owner's comparison of 2026-10-08. turbopuffer's
`rank_by` functions (`Saturate`, `Decay`), Qdrant's score boosting, Weaviate's Boost API and
Vespa's ranking phases all let a query's score depend on a document's attributes: freshness,
popularity, a flag. pstore's only lever today is a weight on a whole leg.

## What is true today

Read from the tree at M61 (`ea44c40`):

- A ranked query (`Plan::Ranked` in `pstore-server/src/lib.rs`) runs the engine's query with
  `top_k`, and `resolve_rows` turns its answer into `(id, score, attributes, $dist)`.
- **Every hit's attributes are already in the answer.** The engine resolves ids and
  attributes in the row round, D-34's fourth, from the blocks holding the hits.
- A score is the fusion's: RRF by default, or raw leg scores under `sum` and `max`.
- `rank_by` orders by one attribute, and cannot be combined with a vector or text (M9e).

## Delta

**A ranked query may carry `boost`. Every leg's limit and the engine's `top_k` become
`window` (spec review: the legs too, or a candidate a leg ranks below `top_k` never reaches
the fusion), on the live, split and `as_of` paths alike. The server adds each candidate's
terms to its score, re-sorts, and cuts to `top_k`. A boosted query therefore costs exactly
what the same query costs unboosted at `top_k = window`, and its depth stays D-34's four.**

1. **The request.** `QueryRequest.boost`:

   ```json
   "boost": {
     "window": 100,
     "terms": [
       {"attr": "popularity", "fn": "saturate", "weight": 0.01, "pivot": 50},
       {"attr": "published", "fn": "decay", "weight": 0.02, "origin": "2026-10-01T00:00:00Z", "scale": 604800, "shape": "exp"},
       {"attr": "price", "fn": "linear", "weight": -0.0001},
       {"attr": "in_stock", "fn": "eq", "weight": 0.05, "value": true}
     ]
   }
   ```

   - `window`: candidates rescored, from `top_k` to 1,000. It defaults to
     `min(1000, max(100, 2·top_k))` (spec review: a window of `top_k` only reorders).
   - A boost needs `top_k` ≤ 1,000, since no window lies below it otherwise.
   - `terms`: 1 to 8. Each names an attribute and a function.
2. **The score.** `final = score + Σ weight · f(x)`, computed in `f64` in term order from the
   candidate's `f32` score in the window-sized query, then returned as `f32`.
   - A term whose `weight · f(x)` is not finite contributes 0.
   - `final` is clamped to ±`f32::MAX` before the cast, so no score is infinite or NaN
     (spec review: a finite weight times a stored float near `f64::MAX` overflows).

   `x` is the candidate's attribute value, as `Engine::resolve_rows` returns it, whatever
   `include_attributes` asks to return:
   - `linear`: `f = x`, for an `int` or `float`.
   - `saturate`: `f = max(x, 0) / (max(x, 0) + pivot)`, for an `int` or `float`; `pivot > 0`.
   - `decay`: `d = |f64(x) − f64(origin)|` for a number, and `d = f64(|x − origin|) / 10⁶`
     seconds for a datetime held in microseconds (in range, so the subtraction cannot
     overflow). Then `f = 0.5^(d/scale)` (`exp`, the default),
     `0.5^((d/scale)²)` (`gauss`), or `max(0, 1 − d/(2·scale))` (`linear`). Each is 1 at the
     origin and 0.5 at `scale`.
     - For an `int` or `float`, `origin` is a number and `scale` is in the attribute's unit.
     - For a `datetime`, `origin` is a string `pstore_format::datetime::parse` reads (RFC
       3339, UTC) and `scale` is in seconds.
     - `scale > 0`.
   - `eq`: `f = 1` when the attribute equals `value` (a string, integer, float or bool), or
     is an array holding an element equal to it; else 0. Equality is a filter's `Eq`:
     numbers by `cmp_numbers`, exactly across int and float (`5 = 5.0`); a string `value`
     against a `datetime` by the instant it parses to; anything else by `Value` equality.

   **A term contributes 0** to a candidate that lacks the attribute or holds a type the term
   does not read: a string or bool or array under `linear`, `saturate` or `decay`; a
   datetime under a numeric `origin`, or a number under a datetime one. Documents of one index
   need not share attributes. The ledger says so, since it is silent by design.
3. **Order and cut.** Candidates are sorted by the returned `f32` `final`, descending, by
   `total_cmp` and a stable sort, so ties keep the engine's order. The first `top_k` are
   returned with `final` as their `score`. `$dist` is
   unchanged: it is the dense leg's, not the boost's.
4. **The window bounds what a boost can lift.** A document the engine ranks below `window` is
   never seen. This is Vespa's phase semantics: exact within the window, and nothing beyond.
5. **Everywhere a ranked query runs.**
   - Multi-query sub-queries may each carry one.
   - A split query (M54) passes `window` as the engine's `top_k`, so its candidates are the
     unpeered server's.
   - `as_of` works as for any ranked query.
6. **Refused `400`:**
   - `window` below `top_k` or above 1,000, or not a whole number, and `top_k` above 1,000;
   - `boost` not an object, or holding a field other than `window` and `terms`.
     `boost: null` is no boost, as serde reads it (code review);
   - a term missing `attr`, `fn` or a field its function needs;
   - no terms, or more than 8;
   - an unknown `fn` or `shape`, or a field the function does not take;
   - a missing, non-numeric or non-finite `weight`;
   - `pivot` or `scale` not above 0;
   - an `origin` neither a finite number nor an RFC 3339 datetime;
   - an `eq` `value` that is not a scalar;
   - an empty `attr`, `id`, or a `$` name;
   - `boost` beside `rank_by` or `aggregate_by`.

**Not changed:** the engine, the format, HEAD, and every query without `boost`.

**Not covered, and the ledger says so:**
- a boost term over a leg's own score, beyond `weight` on the fused score;
- `product` composition;
- an origin of "now", which would make a query's answer depend on the clock it ran at;
- a geo decay (M66).

## Acceptance criteria

1. **Boosts rescore the window.** `boosts_rescore_the_window`, in
   `crates/pstore-server/tests/boost.rs`:
   - A text query and a hybrid one are each run once unboosted with `top_k` equal to the
     window. The test computes rule 2's `final` from those candidates' scores and
     attributes, sorts, and cuts.
   - The boosted query's ids and scores equal that, bit for bit, unfolded and folded.
   - The boost moves at least one document into the top `top_k` from below it.
   - With folded and unfolded rows mixed, `meta.unfolded_hits` counts the returned rows that
     are unfolded, and differs from the unboosted query's (code review).
   - With `include_attributes` naming none of the boosted attributes, the answer's ids and
     scores are the same.
   - Under `as_of` an earlier epoch, the answer is that epoch's window, rescored.
   - In a multi-query, each sub-query applies its own boost, and one without a boost is
     unchanged.
2. **Each function.** `each_boost_function_scores_as_specified`: one document a function, and
   each shape, with the expected `final` written out by hand:
   - `linear` on an int and on a float;
   - `saturate`, including a negative `x`;
   - `decay` with `exp`, `gauss` and `linear`, on a float and on a datetime;
   - `eq` on a string, a bool, a float, an int against a float value, an array, and a
     datetime against a string value;
   - a document lacking the attribute, and one holding another type, scoring 0 for the term:
     a datetime under a numeric origin, a number under a datetime origin, an array under
     `linear`;
   - a term overflowing to infinity contributing 0, and a final beyond `f32::MAX` clamped.
3. **The window bounds the lift.** `the_default_window_is_twice_top_k_and_at_least_100`
   (code review) pins the default at top_k 10 and 60. `the_window_bounds_what_a_boost_can_lift`: a document
   ranked just outside `top_k` but inside `window` is lifted into the answer; the same
   document is absent when `window` equals `top_k`. On a dense leg alone the document is one
   the leg's own cut at `top_k` would have dropped, so the legs' widening is what lifts it.
4. **Refusals.** `boosts_are_refused_at_the_door`: every `400` of rule 6, and the limits
   themselves accepted.
5. **Cost.** `a_boost_costs_its_window`: a cold boosted query is four rounds deep, at one
   segment and at four, with unfolded rows present and without. Its requests and bytes equal
   those of the same query unboosted at `top_k = window`, cold.
6. **Split.** `a_split_boosted_query_answers_as_one_server_does`, in
   `crates/pstore-server/tests/peers.rs`: on M54's fixture, a boosted query sends parts and
   answers exactly as the unpeered server.
7. **Gates:**
   - `./scripts/gates.sh` passes;
   - the mutation sweep of the changed lines misses 0, or each miss is killed by a test named
     in the ledger.

## Test plan

| AC | Seen red first under |
|---|---|
| 1 | `boost` absent (the field unknown, the query unboosted) |
| 2 | each function's formula altered |
| 3 | the engine asked for `top_k` instead of `window`; the legs left at `top_k` |
| 4 | each refusal removed |
| 5 | the engine asked for `top_k` instead of `window` |
| 6 | a split part's dense leg cut below the window on the wire (a split-only fault: both paths share `run`) |
| 1 | `window` not passed on the `as_of` path; the boost read after `selected` |

## RA budget

Depth unchanged. Requests and bytes are exactly the unboosted query's at `top_k = window`
(AC5), which is the honest bound (spec review: not "one read a segment"):
- the row round issues a ranged read per coalesced span of blocks holding a candidate, so
  more spans for a wider window;
- the leg rounds read more as limits grow: the dense leg's oversampled rerank, and the text
  leg's weaker pruning.

CPU: `window × terms`.

## Risks

- **Scale.** RRF scores are about 1/60, so a weight of 1 swamps the fusion. The docs say so,
  and `sum` or `max` fusion gives raw scores.
- **Bytes.** A window of 1,000 rows reads more blocks. The cap bounds it.
- **A multi-query of 16 sub-queries at a window of 1,000** reads up to 16,000 rows in one
  request. Each is billed in `meta.cost`; no extra cap.

## Tasks

- **M62.1** `boost` parsed and applied in `run`, the engine asked for `window`; the docs; the
  tests: AC1–AC7.
