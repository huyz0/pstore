# M170 verification

The spec was amended while implementing and after one code review round (block, two
majors, both taken); both are recorded in the [spec](SPEC.md).

How to run the tests:
- server: `cargo test -p pstore-server --test it text_only::`;
- unit: `cargo test -p pstore-engine --lib query_error_tests`.

Red run: on the parent, with the test file in place and the source changes stashed, four
of the five tests then written failed with `malformed body: missing field vector`, and
`what_a_vector_free_document_cannot_carry_is_refused` passed (the parent refuses every
case), so its red is the mutations below. The unfolded-rows test was added after review;
its red is the mutation "the unfolded rows' width not inferred".

The mutations were applied one at a time by a script and reverted, each run against
`text_only::`. No `cargo mutants` run is claimed for them.

1. `a_vector_free_document_is_written_filtered_ranked_and_projected`: green after; red on
   the parent.
2. `a_vector_free_document_is_patched_and_read_back_after_a_restart`: green after; red on
   the parent.
3. `a_vector_query_against_a_vector_free_index_is_refused_by_name` and
   `a_vector_query_against_unfolded_vector_free_rows_is_refused_by_name`: green after.
   Killed: the engine's refusal removed; removed from the live path; removed from the
   `as_of` path; a `multi` leg not counted as a vector leg; the unfolded rows' width not
   inferred. `only_a_known_width_of_zero_refuses_a_vector_leg` covers the `as_of` path's
   missing schema; the call-site mutation that reads it as width 0 survived the server
   tests, as no server test can build a HEAD older than M7d, so that is OBSERVED-NOT at the
   call site and checked by the unit test on the function.
4. `an_index_takes_vectors_or_none_never_both`: green after; red on the parent. Killed:
   the door's mixed-batch check removed (after review, by the message assertion).
5. `what_a_vector_free_document_cannot_carry_is_refused`: green after. Killed: the
   empty-vector check removed; the named-fields check removed; the metric check removed
   (only by the `euclidean_squared` and `dot_product` cases: under `cosine_distance` the
   engine refuses the zero vector itself); every door check skipped.
6. `an_index_first_folded_with_deletes_alone_still_takes_vectors`: green after, a guard.
   The workspace suite under `scripts/coverage.sh --fail-under-lines 95
   --fail-under-regions 95 --ignore-run-fail -- --skip
   ipv6_clients_refuse_malformed_replies_and_release_wire_capacity --skip
   serialization_failures_never_send_partial_requests_or_responses`: 2,279 passed, 0
   failed; lines 95.75%, regions 96.64% (`provisional`: a 4-core cloud container). The two
   skipped tests need IPv6, which the container lacks; CI runs them.

CI's `mutants` job on the PR timed out on two mutants of `widths_agree` (`==` to `!=` on the
empty-vector check, `!=` to `==` on the width check): each refuses every write, and the
integration tests then wait rather than fail. `widths_agree_takes_one_width_or_none_and_refuses_the_rest`,
a unit test in `pstore-server`'s lib, now fails on both in under a second (both mutations
applied by hand and observed red), so the lib's tests stop the run first.
