# M173 verification

The spec was reviewed in three rounds, recorded in the [spec](SPEC.md); the code in
two, recorded below.

How to run the tests:
- server: `cargo test -p pstore-server --test it -- persistent_types:: uint64:: datetime::
  uuid_bytes::`;
- engine: `cargo test -p pstore-engine --lib -- type_tests:: head::`;
- format: `cargo test -p pstore-format --lib typed::`.

Red run: on the parent, with the server test file in place and every source change in
`pstore-engine`, `pstore-format` and `pstore-server/src` stashed, all six server tests
failed. The engine's and the HEAD's unit tests' red is the mutations below: applied one at
a time by a script against the commands above and reverted, all killed. No `cargo mutants`
run is claimed for them.

1. `a_recorded_type_reads_every_later_write_after_a_restart`: green after; red on the
   parent. Killed: the recorded types not read at the door (in the engine, and in the
   server's reading); a patch's attributes read by the request's declarations alone.
2. `an_unfolded_declaration_reads_the_processs_next_write`: green after; red on the parent.
   Killed: the unfolded map not read.
3. `a_declaration_differing_from_a_known_type_is_refused`: green after; red on the parent.
   Killed: a conflicting declaration accepted.
4. `concurrent_first_declarations_record_the_first_in_lane_order`,
   `an_untyped_row_meeting_a_declaration_is_quarantined_and_patches_are_not`,
   `a_fold_records_the_first_stamp_in_lane_order_and_judges_every_row`,
   `a_patchs_stamp_records_a_type_too`, `a_row_is_judged_by_its_stamp_and_by_its_values`,
   `every_patch_carries_the_kinds_known_for_what_it_sets` and
   `the_recorded_type_wins_and_evicts_the_unfolded_one`: green after; the server tests red
   on the parent. Killed: the last `$types` recorded rather than the first; a contradicting
   value not quarantined; the losing map entry kept; a patch, or a by-filter patch, not
   stamped. The engine test's untyped row comes before the declaring one in its lane, and
   is quarantined, which is the case "only rows after the declaring one judged" would
   miss. A patched row is not judged: the server test's patch of a row holding an untyped
   `owner` applies.
5. `the_summary_answers_the_recorded_types`: green after; red on the parent. Killed:
   `attributes` omitted.
6. `recorded_types_round_trip_after_keys_since`, with the HEAD fixture's live and dropped
   schemas now recording types: green after. Killed: the dropped schemas' types not
   written; the section written with no types; the sentinel read as an epoch. ⚠️ After merging main, which
   carries M152's further-text-fields section after `keys_since` and reads `u64::MAX` as
   absent wherever it appears (debug-asserting it is never an epoch), the types follow that
   section, and the sentinel case is replaced by a HEAD with both sections; the fixture's
   types alone force an empty text-field section; both round-trip. The sentinel's refusal
   is M152's rule now, not this milestone's.
   `a_head_of_every_section_is_refused_cut_anywhere_but_a_section_boundary` now finds 19
   decodable cuts rather than 15: the types force the outcomes, `keys_since` and, after the
   merge, M152's text-field section, so the cut before each is inside the HEAD, not at its
   end.
7. `a_stamp_names_the_known_kinds_a_row_holds_and_reads_back`: green after. Killed:
   `$types` not stripped. A quarantined row's export lists it under `reserved`, beside
   `$fts` and the other reserved names (M25), as the reason the fold refused the row; no
   query, projection or sealed row carries it.
   `a_dropped_indexs_type_reads_no_later_write` and
   `a_drop_after_a_declaration_takes_it_and_one_before_does_not`, from code review: green
   after. Killed: a refusal by a recorded type not re-read; this process's drop keeping its
   declarations; a drop read later not evicting them, or evicting one made after reading it.
   The workspace suite under `scripts/coverage.sh --fail-under-lines 95
   --fail-under-regions 95 --ignore-run-fail -- --skip
   ipv6_clients_refuse_malformed_replies_and_release_wire_capacity --skip
   serialization_failures_never_send_partial_requests_or_responses`, before code review:
   2,353 passed, none failed; lines 95.83%, regions 96.74% (`provisional`: a 4-core cloud
   container, without the two IPv6 tests, which need IPv6). CI's coverage and mutants runs
   on the PR are the evidence for the code as pushed. CI's `scripts/mutants.sh` on the
   first push: 90 mutants, 32 caught, 57 unviable, 1 missed: the guard that keeps a copy
   of a write for the re-read, as `>=`, which re-read HEAD on a refusal by the request's own
   type. `a_refusal_by_the_requests_own_type_reads_nothing` now fails on it, applied by
   hand; CI's run on the fix is the evidence.
   After merging main's M141 (a tenant's buffered rows bounded), whose
   `buffered::a_write_larger_than_the_bound_is_too_large_not_overloaded` and
   `buffered::deletes_reserve_their_tombstones` assert a refusal costs no blob request, the
   door reads the known types only for a write holding an attribute value. Both tests pass
   unedited; the guard forced true fails them, and forced false fails four of
   `persistent_types::`, applied by hand. CI's mutants on that merge missed the guard's
   patch term (`||` as `&&`, and `!` deleted): `a_patch_alone_after_a_restart_is_read_by_the_recorded_type`
   now fails on both, and on the by-filter term's `!` deleted, applied by hand.
   Existing tests: the M171 and M172 server tests that wrote several types under one name
   (`datetime::datetimes_come_back_in_utc`,
   `datetime::a_string_literal_meets_a_datetime_by_instant_and_a_string_by_bytes`,
   `datetime::rank_by_puts_datetimes_between_numbers_and_strings`, `uint64::` through its
   `seeded` helper and `equal_numbers_are_one_group_and_report_the_integer`) now write the
   untyped rows first, and, in their folded passes, fold them before the declaring write,
   as the spec's "one name, one type" says; their unfolded passes still hold every row
   unfolded; `uuid_bytes::rank_by_orders_uuids_then_bytes_after_strings_and_before_arrays`
   and `uuid_bytes::a_datetime_zone_never_rules_out_a_uuid_or_bytes_row_under_the_same_name`,
   which hold two declared types under one name, write those rows by an engine with no
   declaration, as a build before M173 did. Every assertion is kept. The undeclared row of
   `datetimes_come_back_in_utc` stays a string because it is written before the type is
   known; written after, it is now read as a datetime, which is this milestone's delta.
