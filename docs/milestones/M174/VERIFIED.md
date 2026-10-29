# M174 verification

The spec was reviewed in two rounds, recorded in the [spec](SPEC.md). Code review round 1
passed with two minors: a column of no rows is never name-checked (it names nothing, and
the comment now says so), and a bad column now answers before `applied`'s checks, as a
row-form parse error already did. Neither changes behaviour, so no second round.

How to run the tests:
- server: `cargo test -p pstore-server --test it -- upsert_columns:: attributes::
  typed_values::`;
- unit: `cargo test -p pstore-server --lib columns_normalize`.

Red run: on the parent, with `upsert_columns.rs` in place and the source changes stashed,
all five server tests failed (`upsert_columns` an unknown field, a `null` attribute
refused). The unit test does not compile there (`upserts_of` is new); its red is the
mutations below. Mutations, applied one at a time by a script against the commands above
and reverted, all killed; no `cargo mutants` run is claimed for them.

1. `columns_normalize_to_the_rows_documents`: green after. Killed: the column rows placed
   before `documents`; a null attribute kept; `vector` read as an attribute rather than the
   document field; `"vectors": null` refused.
2. `a_batch_in_columns_answers_as_the_batch_in_rows`: green after; red on the parent.
   Killed: the columns appended after the request-level rules read `documents`.
3. `null_in_an_upsert_is_absent_and_in_a_patch_removes`: green after; red on the parent.
   Killed: a null's name not checked before it is dropped; `$` names allowed.
4. `a_bad_column_refuses_the_whole_request`: green after; red on the parent. Killed: a
   longer column tolerated; a non-object `upsert_columns` accepted. A column-name check
   separate from the attributes' survived its mutation, because a null cell's name is
   checked with the row's nulls and any other cell's by `to_document`, so it was removed.
5. `columns_follow_rows_and_both_are_counted`: green after; red on the parent. Killed: the
   column rows placed before `documents`.
6. `every_rule_reads_the_column_rows`: green after; red on the parent. Killed: the columns
   appended after the request-level rules read `documents`.
7. The server suite, `cargo test -p pstore-server` without the two IPv6 tests: green.
   Changed: `attributes::a_value_the_format_cannot_store_is_refused_not_coerced` and
   `typed_values::what_still_has_no_type_is_refused` lose their `null` case, which is now
   accepted and asserted by criterion 3; every other case stays. The workspace suite under
   `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95 --ignore-run-fail --
   --skip ipv6_clients_refuse_malformed_replies_and_release_wire_capacity --skip
   serialization_failures_never_send_partial_requests_or_responses`: 2,427 passed, none
   failed; lines 97.25%, regions 95.89% (`provisional`: a 4-core cloud container, without
   the two IPv6 tests). CI's coverage and mutants runs on the PR are the evidence for the
   code as pushed.
