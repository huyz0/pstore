# M47 — D-34 restated: three round trips to rank, one to fetch what was ranked

**Serves:** [BACKLOG](../BACKLOG.md) row 26, which [M42](../M42/VERIFIED.md) priced and left as
"a decision for a human". **The decision was made by the project's owner on 2026-10-04, in
answer to a direct question:** restate the budget rather than build ids beside codes. M42's
measurements are the evidence.

## ⚠️ This weakens a documented bound, by decision

AGENTS.md says never move a threshold in the weakening direction **to make a check pass**.
This restatement moves D-34's written number from three to three-plus-one. No check is made to
pass by it: the tests have asserted four since M7c, and none of them changes. Row 26 itself
said that "restating D-34 (three for the ranking, one for the results) is what would cross a
line". That line is crossed here by the owner's explicit decision, which no agent made or
could make. Every banner below says so.

## What is true today

- D-34 is written as one number, three sequential round trips, for "any cold user query"
  (engineering-standards) or a query's "sequential round-trip depth" (evaluation-methodology).
- **The code has been four since M7c, enforced exactly:**
  `a_query_costs_four_round_trips_however_many_segments_it_has` asserts 4 at 1, 4 and 8
  segments. The fourth round reads the row blocks holding each hit, for its id and
  attributes, whose address is unknown until the ranking exists.
- **M42 priced the only fix that reaches three** (`cargo run --release -p pstore-engine
  --example id_round`, provisional): ids stored beside a dense leg's codes.
  - It costs +12.7% to +41.9% bytes across M42's settings: +21.2% at p = 8 and gap 0, and
    +35.4% to +41.9% at the production 1 MiB gap.
  - It reaches three only for dense, id-only queries on clustered segments with `rerank`
    `none` or `fast`: not attribute-bearing queries, `rerank: exact`, text or sparse legs, or
    exact-scan segments.

## Delta

**D-34 is restated: a cold user query ranks in at most three sequential round trips, and
fetches the rows it ranked in at most one more. `rerank: exact`, an opt-in, adds one round to
the ranking and is outside this bound, said so wherever the bound is.** Both halves are flat in
the segment count.

- **The latency arithmetic is exceeded, and said so.** The corpus derived three from ~30 ms a
  round trip against a ~100 ms budget. Four rounds is ~120 ms cold, over that figure by one
  round. What the arithmetic chose still holds: a clustered index, whose ranking is three,
  over a graph index, whose ranking is a chain. The fourth round is one fan-out over known
  addresses, the same depth for any index.
- **Mentions of "three" that mean the ranking stay as written**: the index-selection argument
  in `architecture.md`, INDEX.md's conclusion 2, and C-11 in `full-text-search.md`. The ledger
  lists each with its reason.

### What enforces each query shape

| Shape | Bound | Test |
|---|---|---|
| Dense, through the API, ranked and fetched | **4**, exactly, flat in segments | `a_query_costs_four_round_trips_however_many_segments_it_has` |
| Dense ranking, cold from HEAD | 3 | `a_cold_query_from_head_costs_three_round_trips` |
| Ordered, live and `as_of` (engine) | 3, flat in segments | `an_ordered_query_is_three_round_trips_however_many_segments` |
| Filtered aggregation (API) | ≤ 3 | `a_filtered_aggregation_is_three_rounds_deep` |
| Hybrid, text and sparse legs (query crate) | 2 beyond HEAD, so 3 to rank | `a_hybrid_query_is_no_deeper_than_its_deepest_leg` |
| A token filter on a relevance or ordered query (API) | no added round | `a_token_predicate_adds_no_round` |
| `rerank: exact` | **+1 to the ranking**: outside the bound | `exact_rerank_costs_exactly_one_more_round` |

**Not bounded end to end, said so:**
- text, sparse and hybrid queries through the API;
- `as_of` relevance queries through the API;
- `rerank: exact` through the API, which would be five;
- exact-scan segments (below the clustering threshold), which M42 excluded. The engine's
  `search` over one is bounded at ≤ 3 (`a_cold_search_has_a_sequential_depth_of_at_most_three`),
  but no API-level test bounds a ranked-and-fetched query over one.

The first three are bounded in pieces by the table's rows: ranking, then the shared fetch
round. None of the four has a single HEAD-to-response test. They are the ledger's residue.

### Where it is restated

Each place gets the same banner: **"⚠️ D-34 restated by M47"**, then the bound above and the
owner's decision. The places:
1. AGENTS.md's "Never" line, in full, not as a banner.
2. Research corpus: `runtime-and-io.md`'s D-34 block (the decision's source),
   `engineering-standards.md`'s invariant row, `roadmap.md`'s M1 exit line and its
   cross-cutting row, `evaluation-methodology.md`'s depth line, `blob-store-fakes.md`'s D-34
   row, `query-path.md`'s "Three round trips" line, INDEX.md's `query-path.md` and
   `engineering-standards.md` rows, and `slos.md`.
3. Code comments that call the fourth round a breach of D-34, or call D-34 three for the whole
   query:
   - `pstore-query/src/run.rs` ~439;
   - `Answer.ids` in `pstore-engine/src/lib.rs` ~1232;
   - `pstore-server/tests/depth.rs`'s header and test comment;
   - `pstore-engine/tests/invariants.rs`'s header;
   - `end_to_end.rs` ~90;
   - `examples/head_cost.rs` ~126;
   - `pstore-testkit/src/depth.rs` ~5.
   - ⚠️ `query-path.md` also says "~100–150 ms cold". Its banner reconciles that with the
     ~120 ms arithmetic above: four rounds at ~30 ms falls inside it.
4. BACKLOG row 26 is closed.

## Acceptance criteria

1. **Regression guard: every bound above holds, unchanged.** The seven tests in the table
   pass, untouched by this milestone, which changes no code. `./scripts/gates.sh`.
2. **Layer 0 names both halves and the exception.** These all match:
   - `grep -n "three sequential round trips to rank" AGENTS.md`
   - `grep -n "one more to fetch" AGENTS.md`
   - `grep -n "rerank: exact" AGENTS.md`
3. **Every corpus place in the Delta's list 2 carries the banner.**
   `grep -L "D-34 restated by M47"` over these eight files lists none:
   - `docs/research/09-rust-stack/runtime-and-io.md`
   - `docs/research/09-rust-stack/engineering-standards.md`
   - `docs/research/11-design/roadmap.md`
   - `docs/research/10-benchmarks-cost/evaluation-methodology.md`
   - `docs/research/09-rust-stack/blob-store-fakes.md`
   - `docs/research/08-query-engine/query-path.md`
   - `docs/research/INDEX.md`
   - `docs/research/11-design/slos.md`

   `roadmap.md` holds two places, and the ledger names each line.
4. **No stale statement survives in code.** Over `crates/`, each of these matches nothing.
   Each was checked to match before the change (2, 1, 2, 1, 1 and 1 lines), so the
   criterion was red first:
   - `grep -rn "D-34 allows three"`
   - `grep -rn "D-34's three"`
   - `grep -rn "against a budget of three"`
   - `grep -rn "D-34 caps the second"`
   - `grep -rn "allows about three sequential fetches"`
   - `grep -rn "at most three sequential round trips however the index is laid out"`
5. **The objectives gate holds**, with `slos.md`'s rows naming the restated bound.
   `./scripts/gates.sh` runs `check-slos.py`, `check-links.sh` and `build-index.py --check`.
