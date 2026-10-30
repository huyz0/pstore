# M190 verification

The spec was reviewed in one round, recorded in the [spec](SPEC.md), and amended twice while
the tests were written (recorded there). Code review is recorded in the commit body.

How to run the tests: `cargo test -p pstore-server --test it -- vectors_named:: vectors_out::`.

Red run: on the parent any name but `vector` is 400, so all six new tests failed there for
that reason. M179's refusal of `["other"]` now admits it (a named field), so that case
became `["sparse"]`, still refused. Then each mutation below, applied one at a time by a
script and reverted: 28 killed. Two survived at first: `vector` answered when only named
fields were asked under `diversify` (a test now asserts no `vector` key), and `diversify`
cloning rather than taking the vectors when none is answered, which is unobservable, so the
code went back to M179's condition. No `cargo mutants` run is claimed.

1. `vectors_named::a_ranked_query_answers_each_rows_named_fields_in_either_spelling` (300
   rows folded in two segments, 50 unfolded with 1 to 5 `tokens`, restarted, each metric,
   float and base64): green. Killed: a named field not in client space, an unfolded
   multi-vector field reordered, a row's fields read for another row, the fresh segment's
   fields read rather than taken from memory, an unfolded hit's fields taken from the fetch.
2. `vectors_named::an_export_by_id_pages_rebuild_every_field_exactly` (64-row pages,
   rewritten, 20 exact `vector`, 20 `title` and 20 `multi` queries): green. Killed: an
   order's folded fields not in client space or read for another row, the order's names
   unfiltered, an unfolded row's fields left out of the order.
3. `vectors_named::rows_answer_only_the_named_fields_they_hold`,
   `a_field_folded_at_a_varying_count_fails_naming_the_rule` and
   `names_that_cannot_be_projected_are_refused_naming_the_rule`: green. Killed: an empty
   `vectors` object answered, the varying layout answered silently or as a 500 (ranked and
   order), `diversify` without vectors, `vector` answered unasked, each refusal removed, and
   the name bound off by one either way. The engine test
   `vectors::a_hit_whose_row_vanished_does_not_misalign_the_vectors` now pins the fields' pairing too.
4. `vectors_named::a_named_projection_costs_no_round_and_a_request_a_segment` (1 and 4
   segments): green, a ranked query naming `title` as deep as naming `vector`, the order 4
   deep with one more request a segment, M179's pins unchanged. Killed: the named reads
   awaited after the rows' (ranked and order), the order's round skipped for named fields,
   the named fields never read; and (CI's mutants on the PR found it, then a test was
   added and killed it here) `vector` read beside named fields it was not asked for,
   ranked and order. The workspace suite, without the two IPv6 tests, merged
   with main after M162 (49a5df74): 2,597 passed, none failed. `scripts/coverage.sh --changed --fail-under-lines 95
   --fail-under-regions 95 --ignore-run-fail` with those skips, after `cargo llvm-cov clean`:
   lines 97.45%, regions 96.49% (`provisional`: a 4-core cloud container). CI's coverage
   and mutants runs on the PR are the gate.
