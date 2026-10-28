# M171 verification

The spec was reviewed in one round (block, two majors, both taken) and the code in one
(block, two majors, both taken); both are recorded in the [spec](SPEC.md).

How to run the tests:
- server: `cargo test -p pstore-server --test it uuid_bytes::`; `cargo test -p pstore-server --lib typed::`;
- query: `cargo test -p pstore-query --lib -- filter:: condition:: aggregate::`;
  `cargo test -p pstore-query --test it datetime_pruning::`;
- format: `cargo test -p pstore-format --lib -- docs:: typed::`;
  `cargo test -p pstore-format --test it typed_values`.

Red run: on the parent, with the server test file in place and every source change
stashed, all five server tests then written failed (the parent refuses `uuid` and `bytes`
declarations). The format variants were written before the spec passed review, so the
unit tests' red is the mutations below. The mutations were applied one at a time by a
script and reverted, each run against the commands above. All 14 were killed, and two more
after code review. No `cargo mutants` run is claimed for them.

1. `uuids_and_bytes_answer_as_written_before_and_after_a_fold_and_a_restart`: green after;
   red on the parent. Killed: a UUID answered uppercase; bytes answered unpadded; uppercase
   hex refused.
2. `filters_read_a_string_literal_as_the_rows_type`, `uuid_and_bytes_rows_compare_by_their_bytes`
   and `a_boost_and_a_ref_new_condition_read_uuids_as_filters_do`: green after; the server
   tests red on the parent. Killed: a UUID literal compared as a string; a bytes literal
   read as a UUID; the boost's `Eq` structural for UUIDs; after review, the bytes-against-bytes
   arm of `compare` deleted and the boost's bytes arm deleted.
3. `rank_by_orders_uuids_then_bytes_after_strings_and_before_arrays`: green after; red on
   the parent. Killed: the UUID and bytes groups swapped.
4. `malformed_typed_values_are_refused_and_nothing_is_buffered` and
   `each_declared_type_reads_its_spelling_and_refuses_the_rest`: green after; the server
   test red on the parent. Killed: the 4,096 bound off by one.
5. `every_value_tag_round_trips_and_a_short_one_is_refused`, `every_predicate_round_trips`,
   `every_truncation_and_every_unknown_tag_is_none`, and the datetime pruning property
   (`a_segment_with_datetime_zones_prunes_soundly`) with a UUID
   block and a bytes block beside datetimes; `$ref_new` in
   `a_boost_and_a_ref_new_condition_read_uuids_as_filters_do`. Killed: the writer not
   marking the strings flag for UUIDs, and for bytes; the condition codec's UUID tag
   dropped; its bytes length ignored; the block codec's UUID tag dropped; its skip of
   bytes reading nothing.
6. The workspace suite under `scripts/coverage.sh --fail-under-lines 95
   --fail-under-regions 95 --ignore-run-fail -- --skip
   ipv6_clients_refuse_malformed_replies_and_release_wire_capacity --skip
   serialization_failures_never_send_partial_requests_or_responses`, before the code
   review's added tests: 2,300 passed, 0 failed; lines 95.75%, regions 96.67%
   (`provisional`: a 4-core cloud container; the two IPv6 tests need IPv6, which it lacks).
   The extended tests lose no case: the condition codec's unknown-tag case moved from tag
   6, now a UUID, to 8. `uuids_and_bytes_order_structurally_after_datetimes_and_by_their_bytes`
   and `keys_order_and_unify_as_stated` cover the structural and group orders.
   CI's `scripts/mutants.sh` on the first push: 116 mutants, 60 caught, 52 unviable, 3
   missed and 1 timeout. Fixed: the two `Key` arms for UUIDs and bytes survived because
   `Key`'s equality is its order, so the sorted-list assertion could not see a tie; the
   in-group loop now asserts a strict `<` for two UUIDs and two bytes, and deleting either
   arm fails `keys_order_and_unify_as_stated` (run by hand). `parse_uuid`'s `|` to `^`
   was equivalent (the nibbles are disjoint) and its `+=` to `*=` looped forever; it is
   rewritten without a hand-stepped index, and its eight remaining operator mutants were
   applied by hand, each failing `typed::`. No new `cargo mutants` run is claimed locally
   (it is not installed in this container); CI's run on the fix is the evidence.
