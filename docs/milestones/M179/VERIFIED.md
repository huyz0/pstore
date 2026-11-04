# M179 verification

The spec was reviewed in two rounds, recorded in the [spec](SPEC.md). Code review round 1
passed with two minors, recorded in the commit body.

How to run the tests: `cargo test -p pstore-server --test it -- vectors_out::`.

Red run: on the parent `include_vectors` is an unknown field, so the new tests failed there
(the costs measured without it, run on the parent, matched the pinned numbers). Then each
mutation below, applied one at a time by a script and reverted: 21 killed, and one
equivalent (an empty vector filtered twice), so its filter was removed. One mutation, the
metric of an index never folded, survived at first: its new test failed against the code,
which read the metric from rows already stripped of it, and passes after the fix. No
`cargo mutants` run is claimed.

1. `vectors_out::a_ranked_query_answers_each_rows_stored_vector_in_either_spelling` (300
   rows folded, 50 unfolded, restarted, each metric, float and base64): green. Killed: the
   euclidean extra component kept; base64 of big-endian bytes; the ranked path not reading
   vectors.
2. `vectors_out::an_export_by_id_pages_rebuild_the_index_exactly` (64-row pages, rewritten,
   20 exact queries) and `an_index_never_folded_exports_in_its_own_metric`: green. Killed: an
   order's vectors read for the wrong rows; left out for folded or for unfolded rows; the
   metric of an index never folded.
3. `vectors_out::a_row_without_a_vector_answers_none_and_diversify_keeps_its_picks` and
   `what_cannot_be_projected_is_refused_naming_the_rule`: green. Killed: `diversify` taking
   the picked rows' vectors; each refusal removed; the row bound off by one either way, or
   applied by the other rule to an order or a ranked query.
4. `vectors_out::a_projection_costs_no_round_on_a_ranking_and_one_on_an_order` (1 and 4
   segments, a page spanning all four): green, the ranked query as deep as without, the order
   exactly 4. Killed: the order's vector round run when not asked; awaited segment by
   segment. The workspace suite, without the two IPv6 tests, rebased on main after M178 merged
   (9ca8ccdd): 2,580 passed, none failed. `scripts/coverage.sh --changed --fail-under-lines 95 --fail-under-regions 95
   --ignore-run-fail` with those skips: lines 97.45%, regions 96.08% (`provisional`: a
   4-core cloud container). CI's coverage and mutants runs on the PR are the gate.
