# M65 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container with 4 cores. No latency is claimed;
depth, requests and bytes are exact.

Commands:
- `cargo test -p pstore-server --test facets`: 6 tests.
- `cargo test -p pstore-server`

1. **Facets are the aggregation.** `facets_equal_the_aggregation_of_the_same_filter`: over
   folded rows plus an unfolded upsert and delete, grouped and ungrouped, unfiltered and under
   two filters, and at a past `as_of`, `facets` equal the M12 query's response. Seen red with
   `facets` ignored (the response had none).
2. **The ranking is unchanged.** `facets_do_not_change_the_hits`: dense, text, hybrid, a
   boost, a diversify and a highlight return the same `results` with and without facets.
3. **Whole filtered set.** `facets_count_the_filtered_set_not_the_page`: 44 rows counted at
   `top_k = 3` under a diversify, and the filtered count under a filter.
4. **Depth.** `facets_ride_beside_the_ranking`: at one and four segments, folded and with
   unfolded rows, the combined depth is at most the deeper of the two queries' (4) and its
   reads are at most their sum and more than the ranking's. Seen red with the facets awaited
   after the ranking: depth 7 at one segment.
5. **Consistency.** `facets_honour_the_consistency_level`: a `strong` ranked query with facets
   is refused `503` where the aggregation alone is; `bounded` serves the facets and reports
   staleness.
6. **Refusals.** `facets_are_refused_at_the_door`: 13 refusals, each naming `facets`; `top_k`
   10,000 accepted; `null` is no facets.
7. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - **Mutation:** `cargo mutants --in-diff` over the changed lines of `facets.rs` and `lib.rs`,
     `--no-config`, against `pstore-server`'s `facets` and `aggregate` tests: **11 mutants, 9
     caught, 2 unviable, 0 missed**.

**Code review**, one round: no blocking or major findings. **Noted, minor:**
- a run-time error of the facet read (`not_folded`, session coverage) is not prefixed with
  `facets`; status and code are right;
- the returned session token is the ranked read's alone; the facet read's epoch is
  `meta.facets_epoch`, and a token only moves forward, so a later `session` read is safe.

**Not covered**, as the spec states: facets over the ranked window, per-facet filters,
histograms and distinct counts.
