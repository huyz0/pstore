# M191 verification

The spec was reviewed in one round, recorded in the [spec](SPEC.md), and amended while the
tests were written (recorded there). Code review is recorded in the commit body.

How to run the tests: `cargo test -p pstore-server --test it -- vectors_multi:: vectors_named::`
and `cargo test -p pstore-format --test it -- field_spans decode_vectors`.

Red run: on the parent a folded `tokens` is `422`, so the four server tests first written
failed there (4 of 4). Then each mutation below, applied one at a time by a script and
reverted: 26 killed. Three survived at first: the bound read as `>=`, and the bytes summed
over one segment or one field only; the test of the bound's exact byte count was added and
kills all three. The extra round's fan-out given an empty entry per segment survived, and is
equivalent: an empty `get_ranges` plans no request, so it adds no round. A dedupe of the
resolved rows was unobservable (a query's hits and an order's page name each row once) and
was removed. Code review then found the format half's guards unreached through the server:
`format` tests were added, and five mutations of `field_spans` and `decode_vectors` were
killed. No `cargo mutants` run is claimed.

1. `vectors_multi::a_ranked_query_answers_folded_multi_vector_fields_in_either_spelling`
   (300 rows folded in two segments, `title` fixed in the first and varying in the second,
   50 unfolded, restarted, each metric, float and base64): green. Killed: the vectors read
   for another row, the offset table read one row on, the offsets paired with the next
   row, an empty span answered as `[]`, the ranked path's varying fields dropped, the
   vectors decoded at the wrong width.
2. `vectors_multi::an_export_by_id_pages_rebuild_folded_multi_vector_fields_exactly`
   (64-row pages rewritten, 20 exact `vector`, 20 `title` and 20 `multi` queries): green.
   Killed: the order's vectors read for another row, an empty span answered as `[]` in an
   order, the order's varying fields dropped.
3. `vectors_multi::projected_bytes_past_the_bound_are_refused_before_any_vector_is_read`,
   `vectors_multi::the_bound_counts_every_segments_and_fields_bytes_and_admits_its_equal`
   and `vectors_named::a_field_folded_at_a_varying_count_is_answered`: green. A bound of
   300 refuses a ranked query and an order one round shallower than admitted, 10,000
   admits 5 rows, 200 admits 2 folded hits beside 3 unfolded rows, and two segments
   varying in both fields refuse at 2,431 bytes naming 2,432 and admit at 2,432. Killed:
   the vectors read for every candidate, the bound checked after the read, `>` as `>=`,
   the bound removed, the bytes as the largest span, over one segment or one field only,
   the bound unset in the ranked path, the order or the server, and the refusal answered as
   a 500 (ranked and order). M190's test, renamed from
   `a_field_folded_at_a_varying_count_fails_naming_the_rule`, now asserts the field is
   answered. `format`'s `fde::field_spans_locate_only_a_varying_fields_named_rows` and
   `fde::decode_vectors_reads_whole_vectors_of_its_width_only`: green, killing a fixed
   field or a sparse one given offsets, the guard's `||` as `&&`, a row at the end kept, and
   `dims` of 0 unguarded. The guard's redundant `dims` and dedupe were removed.
4. `vectors_multi::a_folded_multi_vector_projection_costs_one_round_more` (1 and 4
   segments): green. A ranked query naming `tokens` is one round deeper than naming
   `vector`, an order naming it 5 deep, `title` alone keeps M190's pins, and the request
   counts are pinned. Killed: a fixed field read in the extra round, a fixed field dropped,
   an order's layouts swapped, the offsets awaited after the rows (ranked and order), the
   vectors' round run one segment after another. The workspace suite, without the two IPv6
   tests, on the final tree: 2,604 passed, none failed. `scripts/coverage.sh --changed
   --fail-under-lines 95 --fail-under-regions 95 --ignore-run-fail` with those skips, after
   `cargo llvm-cov clean`: lines 97.43%, regions 96.03% (`provisional`: a 4-core cloud
   container), measured one edit before the final tree, while a format test still failed on
   a guard since restored. CI's coverage and mutants runs on the PR are the gate.
