# M266: `Highlight` in `compute_attributes`, with `rank_fragments_by`

## Serves
The [turbopuffer gap list](../../comparisons/turbopuffer-gaps-2026-11-15.md), the
"`compute_attributes`" row's `Highlight` and the "`rank_fragments_by` in `Highlight`" row.
The vendor's `compute_attributes` takes `["Highlight", attr]` and
`["Highlight", attr, config]`, the config holding `fragment_by`, `fragment_limit` (default
3), `include_offsets` and `rank_fragments_by`, "how to rank candidate fragments within the
attribute before selecting the top `fragment_limit`. Defaults to the query's `rank_by`."
Shapes are the pinned SDK's, `turbopuffer` 3.1.0 (`types/custom.py`, `ExprHighlight`,
`ExprHighlightWithConfig`; `types/highlight_config_params.py`). Builds on M265 and M159.
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md). No D- or
OQ- id covers it.

## Delta
1. **`["Highlight", attr]` and `["Highlight", attr, config]`** are `compute_attributes`
   values. `config` holds M159's structured keys (`fragment_by`, `fragment_limit`,
   `include_offsets`) and M63's `pre`, `post` and `max_chars`, with M159's rules and
   messages, and `rank_fragments_by`. `fragment_limit` defaults to **3**, the vendor's
   default, not M159's 1.
2. **The value** is the list of fragments M159's `$highlights` returns (`text`, and
   `fragment_range`, `match_ranges`, `array_index` as M159 gives them), or `null` where the
   row has no string at `attr`. So `$computed`'s values are a number, `null`, or such a
   list.
3. **`rank_fragments_by`** is a `rank_by` scoring expression (M265's: `check_limits`, not
   the root rule). Each candidate fragment M159 would consider is scored by it **as if the
   fragment were the row's whole `attr` value**: a document holding only `attr`, scored
   with the index's statistics for `attr`, from the query's snapshot. Fragments are taken
   by score, highest first, ties in M159's order (the earliest), skipping overlaps as M159
   does; a fragment scoring 0 is taken only when none scores more, as M159 takes the first.
   **Marked** are the terms of the expression's `BM25` clauses over `attr`, analyzed by
   `attr`'s analyzer.
4. **The default** `rank_fragments_by` is the query's own text: for a `text` query, a `Sum`
   of `[attr, "BM25", q]` over its legs' query strings `q`. A query with no text leg (a
   vector query) must name one.
5. **Refused `400` before any blob request:** a malformed `Highlight` (not 2 or 3 entries,
   `attr` not a non-empty string, not `id`, not starting with `$`; a config that is not an
   object or holds another key; M159's and M63's own value refusals); a `rank_fragments_by`
   that is not an expression, or has no `BM25` clause over `attr`; a vector query's
   `Highlight` without `rank_fragments_by`. **Refused `400` from the snapshot:** `attr`
   that is not a text field (the `BM25` clause's own refusal, as `rank_by` refuses it), or
   a `pre_tokenized_array` one (M158's refusal: nothing to mark).
6. **The same snapshot, no extra round**, by M265's mechanism: the ranking expression joins
   the computed expressions whose statistics are read beside the query. `attr`'s value is
   in the row fetch already.

**No setting.** Only a query naming `Highlight` in `compute_attributes` changes; M63's and
M159's `highlight` are untouched. That is the settings rule's opt-in case.

**Not changed:** `highlight` (M63, M159), every other `compute_attributes` value, the wire
of a query without it.

## Acceptance criteria
1. **Fragments equal M159's where the ranking cannot differ.**
   `computed_highlight_matches_m159`, in `crates/pstore-server/tests/it/computed.rs`: for a
   text query, `["Highlight", "text", {"fragment_by": "sentence", "fragment_limit": n,
   "include_offsets": u}]` returns exactly the `$highlights` that `highlight: {"attr":
   "text", ...same}` returns, for each `u` and two values of `n`, on rows with at most one
   marked fragment and on a row with nothing marked (both take the first). On a row of four
   equal-length sentences, each marked once, a config naming no `fragment_limit` returns
   three fragments, M159's `fragment_limit: 3`'s. A row without the attribute reports
   `null`.
2. **`rank_fragments_by` orders fragments.** `computed_highlight_ranks_fragments`: one
   row whose sentences each hold one of two terms, the rare term's sentence after the
   common term's. With `rank_fragments_by: ["text", "BM25", "rare common"]` and
   `fragment_limit: 1` the rare term's sentence is returned (its BM25 is higher); with
   `["text", "BM25", "common"]` the common one; under a `Product` weighting the other term
   the order flips. A vector query naming it is answered, marks included.
3. **Depth.** `computed_attributes_add_no_round` gains a text query with a `Highlight`
   naming `rank_fragments_by`: depth equal to the query without.
4. **Refusals.** `computed_highlight_refusals`: each refusal of Delta 5, `400`, naming why,
   the pre-snapshot ones with no blob read.
5. **Gates:** fmt, clippy, the workspace tests, `scripts/gen-clients.py --check` (the
   result row's `$computed` values widen), and the named mutations observed red.

## Test plan
AC1, AC2 and AC4 are red against the parent, which refuses `"Highlight"` as an expression.

| AC | Mutation |
|---|---|
| 1 | the default `fragment_limit` left at 1 |
| 2 | fragments ranked by mark count, ignoring the expression; the fragment scored as the whole row's value |
| 4 | the missing-`BM25`-over-`attr` refusal dropped |

## RA budget
Depth unchanged (AC3). The ranking expression's statistics are M265's: one dictionary read
per segment for `attr`, in the round the query opens its segments, already read when the
query's own text leg is over `attr`.

## Risks
- **CPU:** each row's candidate fragments are tokenized and scored. Bounded by the page
  (at most `MAX_TOP_K` rows), the value's length, and 256 names.

## Tasks
- **M266.1** The spec.
- **M266.2** The evaluator's per-expression scoring of arbitrary documents and its marked
  terms; the highlighter's ranking by given scores; the server's parse and rendering; the
  OpenAPI widening; tests; docs.
