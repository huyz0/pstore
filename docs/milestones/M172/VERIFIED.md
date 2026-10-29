# M172 verification

The spec was reviewed in one round (three majors, all taken) and the code in one (pass, two
minors, both taken); both are recorded in the [spec](SPEC.md) or below.

How to run the tests:
- server: `cargo test -p pstore-server --test it -- uint64:: typed_values::`;
  `cargo test -p pstore-server --lib typed::`;
- query: `cargo test -p pstore-query --lib -- aggregate:: condition::`;
  `cargo test -p pstore-query --test it typed_pruning`;
- format: `cargo test -p pstore-format --lib -- reader:: docs::`;
  `cargo test -p pstore-format --test it typed_values`.

Red run: on the parent, with the server test file in place and every source change
stashed, all six server tests failed (the parent refuses the `uint64` declaration). The
format and query unit tests' red is the mutations below: 21, applied one at a time by a
script against every command above and reverted, all killed. No `cargo mutants` run is
claimed for them.

1. `uint64s_answer_exactly_before_and_after_a_fold_and_a_restart`: green after; red on the
   parent. Killed: `u64::MAX` answered as a float; answered as an `i64`, wrapped.
2. `filters_compare_uint64s_ints_and_floats_by_exact_value`,
   `a_boost_and_a_ref_new_condition_read_uint64s_as_filters_do` and
   `numbers_compare_exactly_across_int_and_float`: green after; the server tests red on the
   parent. Killed: a uint64 against an int compared as `i64`; against a float through
   `f64`; a filter literal above `i64` refused; a boost literal above `i64` read as a
   float; after code review, a boost's numeric function ignoring a uint64; the condition
   codec's tag 8 dropped.
3. `rank_by_facets_and_sums_read_uint64s_as_numbers`,
   `equal_numbers_are_one_group_and_report_the_integer`, `keys_order_and_unify_as_stated`
   and `sums_are_typed_as_stated`: green after; the server tests red on the parent. Killed:
   `5` uint64 and `5` int as two buckets; the float `2^63` reported as a float; a uint64 sum
   not exact above `i64`; a merge forgetting a uint64 was summed.
4. `malformed_uint64s_are_refused_and_nothing_is_buffered` and
   `each_declared_type_reads_its_spelling_and_refuses_the_rest`: green after; the server
   test red on the parent. Killed: a negative or fractional value accepted; the old
   refusal message kept.
5. `every_value_tag_round_trips_and_a_short_one_is_refused`,
   `every_truncation_and_every_unknown_tag_is_none`, `an_unsigned_index_is_framed_exactly`,
   `a_typed_index_is_framed_exactly`, `a_dated_segment_without_uint64s_keeps_its_bytes`,
   `uint64s_round_trip_and_make_a_segment_typed`,
   `mixed_ints_and_floats_prune_soundly_with_zone_maps` and
   `a_block_of_uint64s_below_a_literal_is_pruned`: green after. The flag-2 hash was recorded
   on 95b292f, the parent. Killed: the uint64 zone not recorded; its `min` and `max`
   swapped; flag 3 written with no uint64 zone; a uint64 not marking the segment typed; the
   reader skipping the uint64 table; pruning ignoring the uint64 zone; the block codec's
   skip of tag 8 reading nothing.
6. The workspace suite under `scripts/coverage.sh --fail-under-lines 95
   --fail-under-regions 95 --ignore-run-fail -- --skip
   ipv6_clients_refuse_malformed_replies_and_release_wire_capacity --skip
   serialization_failures_never_send_partial_requests_or_responses`, before code review:
   2,328 passed, 1 failed; lines 95.81%, regions 96.69% (`provisional`: a 4-core cloud
   container; the two IPv6 tests need IPv6, which it lacks). The failure was
   `what_still_has_no_type_is_refused`, which pinned a filter literal above `i64` as
   refused: the behaviour this milestone's delta changes. Its filter cases now assert the
   literal is accepted and compares by value; its write cases, still refused, are kept.
   The extended tests lose no other case: the condition codec's unknown-tag case moved from
   8, now a uint64, to 9; the index framing's unknown flag from 3 to 4; the key test's float
   `2^63`, which the spec now reports as an integer, is asserted so, beside a new `2^64` that
   stays a float. After review, `int_vs_float` lost its range checks, which the `i128`
   cast's saturation makes redundant; CI's coverage and mutants runs on the PR are the
   evidence for the code as pushed.
   CI's `scripts/mutants.sh` on the first push: 96 mutants, 35 caught, 55 unviable, 1
   missed and 5 timeouts. Each now fails a unit test in its own crate, so the suite stops
   early: the `Uint` arm of `Value`'s order (`uint64s_order_by_value`); `normalized`'s lower
   bound as `==` or `<=` (`keys_order_and_unify_as_stated`, with `-2^63` and `-1e19`
   added); an `eq` term's number arm deleted (`an_eq_terms_number_is_read_by_its_range`).
   `literal`'s guard, missed as `true` because an int literal and the same uint64 compare
   alike, is gone: `literal` is now `scalar` with a `u64` fallback on its refusal
   (`a_literal_beyond_i64_is_unsigned_and_the_rest_are_read_as_written`). Each of these
   mutations was applied by hand and failed its test; CI's run on the fix is the evidence.
