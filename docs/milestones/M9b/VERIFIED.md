# M9b — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

1. **Every operator** — `every_operator_admits_exactly_its_documents` (22 filters, unfolded and
   folded, including `Eq null`, `NotEq null`, `id`, and two cross-type comparisons),
   `time_travel_takes_the_filter_too` (`cargo test -p pstore-server --test filters`), and at the
   predicate `a_cross_type_comparison_is_false_not_ordered`
   (`cargo test -p pstore-query --lib`). The server tests were **observed red** before any code
   (the `filters` field was ignored).
2. **Clustered, replicated, far matches** — `a_selective_filter_finds_matches_the_unfiltered_probe_never_reads`
   (`cargo test -p pstore-engine --test filters`): 600 rows, `replicas: 1`; its premise is
   asserted in the test (an unfiltered query for every row misses some of the five farthest
   documents), and the filtered query returns all five.
3. **Every leg, before the limit, and no unsound pruning** — `a_filter_restricts_text_and_hybrid_legs_too`,
   `filtering_happens_before_the_limit` (the two nearest are excluded; `top_k: 2` still returns
   two), `a_filtered_answer_is_the_unfiltered_one_restricted` (single leg, `limit` ≥ rows), and
   `a_negation_keeps_the_rows_that_lack_the_attribute`, and
   `each_leg_is_masked_before_its_own_limit` — ⚠️ added at code review, which found a text leg
   that stopped at its limit and a limit not applied after the mask both passed every other
   test; each hand mutation now fails it (`[]`, and `["d3", "d2"]` for `["d0", "d3"]`). The hand mutation `Not` pruning as
   `!could_admit` was seen failing both `a_negation_keeps_the_rows_that_lack_the_attribute` and
   `a_not_never_prunes_because_a_zone_says_nothing_of_rows_without_the_attribute`; the pruning
   rule's edges are `integer_comparisons_prune_at_their_exact_edges`,
   `and_prunes_if_any_child_does_or_only_if_all_do` and `a_missing_zone_never_prunes`.
4. **No round trip** — `a_filter_adds_no_round_trip`: the same `DepthCounting` depth with and
   without a filter on the clustered segment.
5. **Refusals** — `a_malformed_filter_is_refused` (12 shapes), and an attribute named `id` in
   `a_value_the_format_cannot_store_is_refused_not_coerced` (`--test attributes`).
6. **Gates** — the mutation sweep over the diff,
   `./scripts/mutants.sh --check . --in-diff <the staged diff>` with whole-workspace tests per
   mutant: **77 tested in 48m, 53 caught, 21 unviable, 3 missed**, each resolved: the unused
   function query_rows deleted (the engine calls its filtered twin); the exhaustive leg's
   `oversample: 1` deleted as redundant (with `k` = rows the ladder keeps every candidate
   anyway); the `"Not"` guard killed by a new malformed case, `["Nope", <filter>]`, seen red
   under that mutant. The code-review fixes after it only delete code or add tests.
   `./scripts/gates.sh` on the final tree: all fifteen PASS. Review: spec one round (block:
   criterion 3 described post-filtering, pruning under `Not` unstated, `id` ambiguous — all
   amended); code two rounds (block on the untested text-leg exhaustiveness, then pass).
   ⚠️ A reviewer's hand mutant was staged by accident mid-review and caught by `fmt --check`
   before any commit; the arm was restored and `each_leg_is_masked_before_its_own_limit`,
   which kills that mutant, passes on the committed tree.
