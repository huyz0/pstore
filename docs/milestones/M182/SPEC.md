# M182: exact scoring expressions: `BM25`, `Sum`, `Max`, `Product` and filter-as-score

## Serves
Workstream E of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Represent
BM25, Sum, Max, Product (both forms), filter score ... as a typed expression tree.
Whole-corpus results match a small exhaustive oracle, including zero-score exclusion". The
[comparison](../../comparisons/turbopuffer-2026-10-25.md)'s `Sum`, `Max`, `Product` and
"Filter as score" rows. No D- or OQ- id covers it. `Attribute`, `Saturate`, `Decay` and
`Dist` are the next milestone's: they bring signed values, which this one never produces.

The vendor's [query reference](https://turbopuffer.com/docs/query), read 2026-10-29:
- `["Sum", [...]]` sums its clauses' scores, and `["Max", [...]]` takes the largest. They nest.
- `["Product", w, clause]` and `["Product", clause, w]` multiply by `w`. "The weight must be
  non-negative."
- A filter inside `rank_by` scores 1 when it matches and 0 otherwise.
- `["field", "BM25", query]` takes a string or a token array, with optional
  `{"last_as_prefix": bool}` (the OpenAPI `Bm25ClauseParams`).
- "Documents with a score of zero are excluded from results." No nesting limit is published.

## Delta
1. **`pstore_query::Expr`**, a typed tree:
   - `Bm25 { field, query, last_as_prefix }`, where `query` is a string or a token array, as
     a text leg's (M158, M180);
   - `Sum(Vec<Expr>)` and `Max(Vec<Expr>)`, each with 1 to 16 clauses;
   - `Product(f32, Box<Expr>)`;
   - `Filter(Predicate)`, which scores `1.0` when the predicate admits the row and `0.0`
     otherwise.
   - At most **64 nodes** and a **depth of 8**, with the root at depth 1.
   - A weight is `0` or in `[1e-3, 1e3]`, a deliberate narrowing of the vendor's
     "non-negative". A tree is evaluated in `f64`, from each BM25 leaf's `f32` widened, and
     rows are kept and ordered by that `f64`. The answer's `score` is the wire's `f32`, as
     every ranked row's is (`ResultRow.score`), narrowed from it. With eight weights at most
     `1e3` and sixteen-way sums, that stays far below `f32::MAX`; at the low end a product of
     eight is at least `1e-24`. A positive leaf is a filter's `1.0` or a BM25 score of at
     least about `0.5/N²`: idf is at least about `0.5/N`, and the length factor at least
     about `1/N`. For any `N` below `1e9` rows, that keeps every kept row's score at or above
     `5e-43`, which `f32` holds as a positive (subnormal) value.
2. **A row's score.**
   - `Bm25` is the score the text leg `{field, query, last_as_prefix}` gives that row: the
     same analyzer, `k1` and `b`, and the same global statistics. Those are summed over every
     segment's term dictionary for the field and the index's unfolded rows, exactly as the
     leg sums them. A row the leg does not hit scores 0. Those statistics count every sealed
     row of each segment whose dictionary carries the field: deleted rows, shadowed old
     versions, and rows lacking the field (a length of 0) included. A segment where no row
     has text under the field (a non-empty string, M150) carries no dictionary for it and
     adds nothing. The unfolded
     rows add their own by the same rule, as the fresh segment's dictionary does.
   - `tf` and the row's length come from re-analyzing the row's stored value of the field,
     with the function the segment builder uses (`build_with`). So `tf` and the length are
     the postings' and the fieldnorm's by construction.
   - `Sum` adds, `Max` takes the largest, and `Product` multiplies by the weight.
3. **The answer.**
   - Every row the query's `filters` admit (all rows when there are none) is scored, less
     deleted and shadowed rows. The unfolded rows are scored too.
   - Rows scoring exactly `0.0` are dropped. Every score is `>= 0` here, so a row is kept
     exactly when its tree reaches a positive leaf through positive weights. A BM25 leaf is
     the leg's `f32`: a term the leg scores exactly `0` (an idf that rounds to `0` in `f32`,
     for a term in nearly every row of a corpus above about `8e6` rows) is `0` here too.
   - The kept rows are ordered by score descending, then id ascending. `offset` rows are
     skipped and `top_k` returned, each with its `score`. Attributes are projected as a
     `rank_by` order projects them.
4. **Execution: a whole-corpus scan,** as the `rank_by` order (M9e) is.
   - Round 1: the HEAD.
   - Round 2: in one fan-out, each segment's footer, delete vector and, for each BM25 field
     the segment carries, its term-dictionary sidecar.
   - Round 3: each segment's admitted blocks, in one coalesced plan.
   - No posting list is read.
   - Rows are selected into `offset + top_k`, never sorted whole.
   - This is the exact path. A text leg and native `boost` stay what they are, and nothing
     relabels them as expressions.
5. **HTTP.** `rank_by` takes an expression, in the vendor's spelling:
   - `["Sum", [e, ...]]`, `["Max", [e, ...]]`, `["Product", w, e]` and `["Product", e, w]`;
   - `[field, "BM25", query]` and `[field, "BM25", query, {"last_as_prefix": b}]`;
   - and any filter (`[attr, op, value]`, `["And", [...]]`, `["Or", [...]]`, `["Not", f]`),
     parsed exactly as `filters` parses one and bound to the index's analyzers.
   - A value that is one `[attribute, "asc" | "desc"]` pair, or a list of such pairs, stays an
     M9e/M140 order, unchanged. Every other array is an expression.
   - A BM25 `field` must be a full-text field of the index. The primary is named `text`.
   - The answer's shape is the order's, with `score` on each row.
6. **Refused `400`:**
   - a weight that is negative, NaN, infinite, not a JSON number, or outside `0` and
     `[1e-3, 1e3]` (the vendor accepts any non-negative weight);
   - an empty or over-long clause list; more than 64 nodes; depth over 8;
   - a BM25 field that is not full-text; a `last_as_prefix` that is not a bool; an unknown
     params key;
   - an expression beside `vector`, `multi`, `text`, `field`, `exact`, `fusion`, `boost`,
     `aggregate_by` or `highlight`;
   - `top_k + offset` over 10,000, as an order.

**Not changed:** `rank_by` orders; text legs; fusion; `boost`; every format.

**Not covered:**
- `Attribute`, `Saturate`, `Decay`, `Dist` and numeric literals (the next milestone);
- pruning by upper bounds (WAND), which the plan puts after the oracle;
- splitting an expression query across servers: it is answered by the server that receives it;
- `compute_attributes` (F).

## Acceptance criteria
1. **Oracle.** `expressions_match_the_oracle`, in `crates/pstore-query/tests/it/`.
   - The corpus: 3 segments of 30 rows, with two text fields and a string tag. One field is
     English with stemming and `remove_stopwords`, and its rows hold stopwords. The other is
     a `[]string` under the default analyzer with `max_token_length: 6`, and its rows hold
     longer tokens. A few rows lack a field, and each segment has deleted rows.
   - 300 trees from a seeded generator, over every node kind and up to depth 5. Weights
     include 0, and the tree includes `last_as_prefix` and token-array leaves.
   - The reference scores each row from the raw strings alone:
     - `analyze` for terms;
     - its own BM25 in `f64`, over statistics it counts itself, by Delta 2's rule (every
       sealed row, deleted ones included);
     - its own tree walk;
     - the filter's `admits`.
   - The kept set must equal the reference's positive set **exactly**, and each score must be
     within `1e-4` relative.
   - The order is checked wherever the reference's adjacent scores differ by more than
     `1e-4` relative; equal scores must come by id.
2. **A lone BM25 clause is the text leg.**
   - `a_bm25_clause_is_the_text_leg`: over the same corpus, for 20 queries per field, with and
     without `last_as_prefix`.
   - The leg runs alone under `Sum` with weight 1, so its scores are BM25's.
   - `rank_by [field, "BM25", q]` returns the leg's hits as a set, with scores within `1e-5`
     relative. The order agrees wherever adjacent scores differ by more than `1e-5`
     relative. Within a tie (a prefix-only row scores exactly `1.0`) it is by id, where the
     leg's is by segment and row.
3. **Zero is excluded.**
   - `zero_scores_are_dropped`: a row matching no clause, one under `Product 0`, and one
     admitted by a filter clause that is `Not` of itself are absent.
   - A filter-only expression returns exactly the filter's rows, each scoring `1.0` times
     its weights.
4. **HTTP.** `scoring_expressions_over_http`, in `crates/pstore-server/tests/it/`:
   - the vendor's two documented examples answer as their definitions say;
   - both `Product` forms are equal;
   - unfolded rows are scored before a fold, and the answer equals the one after it;
   - `filters` restrict the scored rows;
   - `offset` pages;
   - an M140 order pair is still an order;
   - every refusal in Delta 6 is answered `400`.
5. **Depth.** `an_expression_query_takes_three_rounds`: the depth-counting store sees 3
   sequential rounds, cold, and no posting-list range.
6. **Gates:** fmt, clippy, the workspace tests (the two IPv6 tests NOT-RUN in this
   container), and the named mutations below each observed red.

## Test plan
Each test is seen red first: `rank_by` refuses an expression on the parent.

| AC | Mutation |
|---|---|
| 1 | `Max` as `Sum`; `Product` ignoring its weight; a filter leaf scoring 0 always; `tf` counted once per term; deleted rows scored; the length taken from the row's raw tokens rather than its terms |
| 2 | the prefix's 1.0 added per matching term; the statistics leaving out rows that lack the field |
| 3 | `> 0.0` as `>= 0.0`; a tie broken by row rather than id |
| 4 | the unfolded rows left out of the statistics; the order pair read as an expression; a negative weight accepted; `Product`'s reversed form refused |
| 5 | posting lists fetched for BM25; the dictionaries fetched in a round of their own |

## RA budget
W 0. List 0. Rseq 3, as a `rank_by` order: the HEAD; then footers, delete vectors and the
dictionaries of the BM25 fields; then blocks. Rpar per segment: the footer, its delete
vector, one dictionary sidecar, and one coalesced block plan. Bytes: every admitted block,
the scan's honest cost, stated in `docs/deploy.md`.

## Risks
- **Float order.** The scan sums a row's BM25 terms in another order than the leg does, so
  the last bits may differ. AC2 bounds that at `1e-5`. Kept sets cannot differ, because
  every positive leaf is `> 0`, a weight is `0` or at least `1e-3`, and `f64` holds every
  product of eight. The reported `f32` is a narrowing; Delta 1 bounds it away from `0` and
  infinity.
- **Statistics drift.** If the unfolded rows' statistics were computed differently from the
  fresh segment's dictionary, scores would drift before a fold. AC4's before-and-after
  equality reveals it.
- **Cost.** Every admitted block is read. A broad expression over a large index costs a full
  scan, as an unfiltered `rank_by` order does; `filters` narrow it.

## Tasks
- **M182.1** The spec, reviewed.
- **M182.2** `Expr`, the scan, the engine call, HTTP, tests, docs.
