# M193 verification

The spec was reviewed in one round, recorded in the [spec](SPEC.md), and amended while the
tests were written (recorded there). Code review is recorded in the commit body.

How to run the tests: `cargo test -p pstore-server --test it -- vectors_sparse:: vectors_named::
vectors_out::`, `cargo test -p pstore-format --test it -- sparse_rows::`,
`cargo test -p pstore-query --test it -- sparse_rows::` and
`cargo test -p pstore-engine --test it -- vectors::`.

Red run: on the parent, with only the hidden hook stubbed so the tests compile, `"sparse"` in
`include_vectors` is `400`, so the five server tests failed there (5 of 5). The format, query
and engine tests call what the parent does not have (`rows_section`, `decode_pairs`,
`Segment::sparse_spans`, `Projection::sparse`, `query_split_projected`'s `sparse`) and do not
compile on it. Then each mutation below, applied one at a time by a script and reverted: 29
killed. Three survived at first, and a test was added for each: the answer's pairs left in
the order written (a server row is a JSON object, so only the engine's answer shows it), the
order's offsets awaited after the page's `vector` (an order naming `sparse` alone makes no
other request in that round), and a corrupt row's pairs answered as none. No `cargo mutants`
run is claimed.

1. `vectors_sparse::a_ranked_query_answers_each_rows_sparse_pairs_as_stored` (300 rows
   with `vector`, `sparse` and `tokens` folded in two segments, 50 unfolded, restarted,
   float and base64, a signed zero) and
   `vectors::sparse_pairs_answer_in_key_order_rounded_folded_or_not`: green. Killed: the
   pairs recorded for the next row, the offset table off by 4, the weight decoded at the
   wrong bytes, a key's length off by one, a weight written off its f16, unfolded weights
   unrounded, the answer's keys unsorted, an empty row answered as `{}`, decoding stopped
   after one pair, the ranked path never asking for the pairs, the section never written.
   `sparse_rows::the_section_is_an_offset_table_then_each_rows_pairs_in_key_order` and
   `sparse_rows::decode_pairs_refuses_bytes_that_are_not_whole_pairs` (format): green,
   killing the section's keys out of byte order.
2. `vectors_sparse::an_export_by_id_pages_rebuilds_the_sparse_field_exactly` (64-row pages
   rewritten; 20 sparse and 20 exact `vector` queries; a sparse-only index of 20 rows):
   green. Killed: the order never asking for the pairs, unfolded order rows without them.
3. `vectors_sparse::the_pairs_count_toward_the_bound_with_the_varying_fields`,
   `vectors_sparse::a_segment_folded_without_the_section_refuses_until_rewritten`,
   `vectors_named::names_that_cannot_be_projected_are_refused_naming_the_rule`,
   `vectors_out::what_cannot_be_projected_is_refused_naming_the_rule`, and
   `sparse_rows::a_pages_pairs_are_read_and_unreadable_ones_refused` (query) and
   `sparse_rows::sparse_spans_locate_each_rows_pairs_and_say_when_the_section_is_absent`
   (format): green. Killed: the pairs read for every candidate, the pairs left out of the
   bound, `>` as `>=`, an old segment answered as no field, its rows not recorded as
   unreadable, refused only for rows holding spans, the refusal answered as a 500 (ranked
   and order), the hidden hook ignored, `sparse` counted toward the name cap, `sparse`
   admitted twice, a corrupt row's pairs answered as none, an empty span read. A
   compaction has no path of its own: it builds through the fold's `assemble`, so the
   section never written is the compaction dropping it too, and the test's compaction of
   an old segment with a new one answers every row's pairs.
4. `vectors_sparse::projecting_sparse_costs_the_same_one_round_more` (1 and 4 segments):
   green. A ranked query naming `sparse` is one round deeper than naming `vector`, `sparse`
   with `tokens` the same depth, an order naming `sparse` (alone, or with `vector`) 5 deep,
   and the request counts are pinned. Killed: the ranked offsets awaited after `rows_at`,
   the order's offsets awaited after its vectors. M178's pins, in
   `sparse::a_sparse_query_costs_what_a_dense_one_does`, are unchanged and green in the
   suite.

Gates on the final tree: `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`
clean. The workspace suite, without the two IPv6 tests: 2,672 passed, none failed.
`scripts/coverage.sh --changed --fail-under-lines 95 --fail-under-regions 95
--ignore-run-fail` with those skips, after `cargo llvm-cov clean`: lines 97.41%, regions
96.08% (`provisional`: a 4-core cloud container). CI's coverage and mutants runs on the PR
are the gate.
