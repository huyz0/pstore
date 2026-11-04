# M178 verification

The spec was reviewed in two rounds and amended in implementation and in code review, all
recorded in the [spec](SPEC.md). Code review round 1 (changes, three majors, all taken) and
round 2 (changes, one major, taken: `facets` beside a sparse leg, now tested; one minor, a
comment, taken).

How to run the tests: `cargo test -p pstore-server --test it -- sparse::`, and
`cargo test -p pstore-index --test it -- sparse::impact_encodings_are_measured`.

Red run: on the parent `sparse` is an unknown field, so every test in `sparse::` failed
there, criterion 3's on its messages. Then each mutation below, applied one at a time by a
script against the server command and reverted: all 20 killed. The `facets` acceptance was
seen red by removing it. No `cargo mutants` run is claimed.

1. `sparse::a_sparse_query_ranks_as_an_exhaustive_dot_product_over_rows_and_columns` (300
   rows over 2,000 keys, as rows and as columns, folded and restarted, 50 queries against an
   f32 oracle over f16-rounded weights): green. `impact_encodings_are_measured` asserts F16,
   its floors unchanged. Killed: U8 stored; the leg's weights or keys dropped; the `sparse`
   column read as an attribute.
2. `sparse::a_sparse_leg_fuses_last_and_is_filtered`: green; each leg's first row is in the
   fused answer, and a filter admitting 40 rows answers a full top 20 of admitted rows.
   Killed: the leg placed first; the filter applied after the limit.
3. `sparse::what_no_sparse_vector_spells_is_refused_naming_the_rule`,
   `a_key_twice_is_its_last_weight_and_a_patch_keeps_the_field` and
   `a_sparse_leg_takes_no_order_aggregation_or_sum_beside_text`: green; 1,025 keys answer
   413 by the body's shape bound (amended), exactly 1,024 and each other bound itself are
   admitted, and `facets` beside a sparse leg answer 200. Killed: the key and weight bounds
   off by one or removed; an empty vector admitted; `sparse` not reserved; a patch row or
   column admitted; a patch dropping the field; the document's field ignored; `rank_by`,
   `aggregate_by` and `sum` beside `text` admitting the leg.
4. `sparse::a_sparse_query_answers_unfolded_rows_nothing_for_absent_keys_and_no_dense_leg`:
   green. Killed: the unfolded write's dictionary dropped.
5. `sparse::a_sparse_query_costs_what_a_dense_one_does`: green, depth 4 (three rounds to
   rank, one for the rows) and 5 requests, pinned. The workspace suite, without the two IPv6
   tests, rebased on main after M177 merged (36c59bb5): 2,572 passed, none failed. `scripts/coverage.sh --changed
   --fail-under-lines 95 --fail-under-regions 95 --ignore-run-fail` with those skips: lines
   97.46%, regions 96.08% (`provisional`: a 4-core cloud container). CI's coverage and
   mutants runs on the PR are the gate.
