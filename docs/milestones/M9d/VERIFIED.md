# M9d — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

1. **Each metric against brute force** — `each_metric_ranks_and_measures_as_brute_force_does`
   (`cargo test -p pstore-server --test distance`): 60 documents whose norms spread 20x, three
   queries each, `exact`, unfolded and folded, `$dist` within `1e-3 + 1e-3·|d|` and the top 10
   by the metric's order; `a_text_only_hit_carries_no_dist_and_a_dense_one_does`. **Observed
   red** on the M9c.2 server, as were the other four tests in the file. ⚠️
   `attributes_are_absent_unless_asked_for` (M9a) pinned a row's keys as exactly `id, score`;
   it now pins `$dist, id, score`, which is this criterion's specified change and just as exact.
2. **Clustered recall** — `a_clustered_segment_recalls_under_every_metric`
   (`cargo test -p pstore-engine --test distance`): 600 rows in 12 clusters, norms spanning 10x,
   20 rows a list, the probe asserted non-exhaustive; recall@10 over 20 queries measured
   **0.99** (cosine) and **0.955** (euclidean) against a floor of 0.8 -- `provisional`. Each
   query's `exact` answer is asserted equal to brute force. The dot-product probe spec review
   proposed was built and measured lower (SPEC.md), so it was not taken.
3. **Base64 and refusals** — `a_base64_vector_is_the_same_vector`,
   `malformed_vectors_metrics_and_reserved_names_are_refused` (not base64, a ragged length, a
   NaN, an unknown metric, a zero cosine vector as a document and as a query, a `$` attribute),
   and `a_vector_too_large_to_measure_is_refused` -- ⚠️ added at code review, which found a
   finite vector whose squared norm overflows `f32` stored as zeros under cosine and with a
   `-inf` component under euclidean; **observed red** before the refusal.
4. **One metric per index** — `a_second_metric_is_refused_at_every_rung_the_width_is` (the
   unfolded rows, then the cached schema; the index's rows unchanged; `GET` reports the metric
   and the client's width in criterion 1's test), and
   `a_fold_rejects_a_second_metric_even_in_a_new_index` -- two engines' first rows, one
   rejected and counted, and no `$metric` sealed -- seen red with the new index's reject pass
   skipped. `the_metric_a_row_carries_is_never_returned_or_filtered_on` (code review: the
   fresh view's strip was untested) is seen red with that strip removed -- `"$metric": 1` came
   back as an attribute of an unfolded row. HEAD's section: `the_metric_survives_a_head_round_trip` and
   `a_truncated_head_is_refused_at_every_length_but_a_section_boundary` (now five cuts).
5. **`exact` adds no round trip** — `exact_adds_no_round_trip`: equal `DepthCounting` depth with
   and without it on a clustered segment, under `dot_product` and `euclidean_squared`; every
   `dot_product` request is unchanged (`--test cost` and the rest of the suite pass untouched).
   `exact_reaches_the_dense_leg` (`cargo test -p pstore-server --lib`; code review: no API
   corpus is large enough to cluster, so no API test could tell `exact` dropped from forwarded)
   is seen red with `exact: false` hard-wired.
6. **Gates** — the mutation sweep over the diff,
   `./scripts/mutants.sh --check . --in-diff <the M9d diff>` after code review's fixes:
   **316 tested in 66m, 157 caught, 158 unviable, 1 missed**. The unviable are type errors
   (`Default` for `Hit`, `Value`, `Document` and the like), none a failed build for want of
   disk. The miss, `x * x` as `x + x` in `transform_query`, was a huge query vector under
   `euclidean_squared` untested: `a_vector_too_large_to_measure_is_refused` now queries one too,
   and `--check transform_query` re-swept **7 of 7 caught**. Code review: two rounds (four minors, each
   fixed with a test seen red; then pass). `./scripts/gates.sh` on the committed tree: all
   fifteen PASS. ⚠️ A first run failed one test because a reviewer's hand mutation was in the
   tree while it ran; the tree was confirmed restored and the whole run repeated.
