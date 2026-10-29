# M175 verification

The spec was reviewed in two rounds, recorded in the [spec](SPEC.md). Code review round 1
passed with three minors, all taken: the key order unit test now holds null elements; the
bundle encoding round-trips the fixture; criterion 4 names `AnyGt`, the literal tested.

How to run the tests:
- server: `cargo test -p pstore-server --test it -- null_elements:: arrays:: upsert_columns::`;
- format: `cargo test -p pstore-format`;
- query: `cargo test -p pstore-query --lib condition::`.

Red run: on the parent, with `null_elements.rs` in place and no source change, the first
three server tests failed (a null element refused); the fourth, criterion 4's, passed, as a
regression guard does. The unit tests' red is the mutations below: applied one at a time by
a script against the commands above and reverted, all 14 killed. No `cargo mutants` run is
claimed for them.

1. `null_elements_answer_in_place_everywhere`: green after; red on the parent. Killed: the
   door refusing a null element, undeclared or declared; `Kind::holds` refusing one; a null
   answered as anything but `null`.
2. `a_null_element_matches_nothing`: green after; red on the parent. No filter code changed:
   a null element is a value `compare` cannot order, which it already reads as no match.
3. `a_null_element_groups_as_itself_and_last`: green after; red on the parent. Killed: a
   null element keyed as absent; sorted first; dropped from the reported bucket.
   `aggregate::tests::keys_order_and_unify_as_stated`, extended from code review, kills the
   last two as well.
4. `what_holds_no_null_element_is_refused`: green before and after, a regression guard.
   Killed: none of this milestone's code refuses these; `arrays::` holds the literal cases.
5. `docs::tests::every_value_tag_round_trips_and_a_short_one_is_refused` and
   `a_projection_keeps_exactly_what_it_names` (their fixture now holds null elements),
   `a_null_outside_an_array_is_refused`, `typed_values::a_null_is_storable_only_as_an_array_element`,
   `typed::tests::a_kind_names_itself_and_holds_its_own_values`, and
   `condition::tests::every_predicate_round_trips` and `anything_else_is_none`: green after.
   The first also round-trips the bundle encoding, from code review.
   Killed: tag 9 written as another tag, in blocks and in conditions; tag 9 decoded or
   skipped at the top level, in blocks and in conditions; tag 9 not skipped;
   `check_storable` storing a top-level null. From CI's mutants run on #21: the top-level
   refusal's own arm deleted, in decoding or in skipping, which fell through to "unknown
   value tag"; `a_null_outside_an_array_is_refused` now names the message, and both fail it. After rebasing onto M141's buffer bound, `Value::footprint`
   counts a null element as its tag, 1 byte;
   `footprint::every_value_kind_is_counted` asserts it.
6. The workspace suite, without the two IPv6 tests: 2,513 passed, none failed, on main after M181 (#18). Changed:
   `arrays::what_an_array_cannot_hold_is_refused` and
   `upsert_columns::null_in_an_upsert_is_absent_and_in_a_patch_removes` lose their
   `[1, null]` refusal, accepted now and asserted by criterion 1; every other case stays.
   The same suite under `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95
   --ignore-run-fail` with those skips, before code review: lines 97.25%, regions 95.89%
   (`provisional`: a 4-core cloud container). CI's coverage and mutants runs on the PR are
   the evidence for the code as pushed.
