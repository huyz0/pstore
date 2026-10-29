# M176 verification

The spec was reviewed in two rounds and amended during implementation; code review round 1
blocked with three majors, all taken, and its minors; all recorded in the [spec](SPEC.md).

How to run the tests:
- server: `cargo test -p pstore-server --test it -- typed_ids::`;
- format: `cargo test -p pstore-format --lib id::`;
- query: `cargo test -p pstore-query --lib a_string_literal_prunes`.

Red run: on the parent, with `typed_ids.rs` in place and no source change, all five server
tests then written failed: an integer id was refused as malformed, and `schema.id` was
undeclarable. The unit tests' red, and the tests added later (`a_later_fold_records_no_kind`,
the patch cases of criterion 3, the zone cases, and those from code review), are the
mutations below: applied one at a time by a script against the three commands above and
reverted, all 28 killed. Those on code that code review's fixes rewrote (the server's id
reading) were run again after them; the rest ran on the tree just before, whose lines the
fixes did not touch. No `cargo mutants` run is claimed for them.

1. `uint64_ids_answer_exactly_and_order_as_numbers`: green after; red on the parent. Killed:
   the form in decimal, or unpadded hex; the fusion tie ordered by the id as answered; a
   filter comparing the held string; an answer, or an `affected_ids` entry, not decoded.
2. `uuid_ids_are_one_row_whatever_their_case`: green after; red on the parent. Killed: a uuid
   kept in its written case; `$ref_new: "id"` giving the held string; a uuid answered raw.
3. `ids_not_of_the_index_kind_are_refused` and
   `a_dropped_indexs_kind_is_read_again_before_it_refuses`: green after; the first red on the
   parent. Killed: the server admitting an id of another kind, or a write of two kinds; the
   engine's door rung for an index with no schema; a NUL-led string id admitted; a declared
   `id` kind ignored, `string` included, or any other kind accepted; an engine message naming
   the held form; no re-read before a refusal by a held kind; the ids rewritten before it;
   from CI's mutants on #23, the re-read guard's `||` as `&&`, which re-read HEAD before a
   final refusal: the mixing case now asserts it reads nothing.
4. `concurrent_first_writes_record_the_first_kind` and `a_later_fold_records_no_kind`: green
   after; the first red on the parent. Killed: the kind taken from the last upsert; recorded
   by a fold that did not create the schema; no quarantine of another kind; the quarantine
   export answering the held form.
5. `the_summary_answers_the_id_kind`: green after; red on the parent. Killed: `id` listed in
   `schema.attributes`; `schema.id` always `string`. Its M147 case was added after rebasing
   onto main with M147, and passes with no source change: the kind is recorded only by the
   fold creating a schema, which an empty index never has. Unit tests:
   `id::tests::a_typed_form_orders_as_its_values_and_answers_them` and
   `anything_else_is_a_string` (killed: hex digits of any case or sign read as a form;
   every string read as a string id), and
   `filter::tests::a_string_literal_prunes_a_datetime_zone_at_its_exact_edges`, extended
   (killed: a numeric zone pruning an id literal). The workspace suite, without the two
   IPv6 tests, on main after M175 and M181 (#21, #18): 2,522 passed, none failed; no
   existing test changed. `scripts/coverage.sh --changed --fail-under-lines 95
   --fail-under-regions 95 --ignore-run-fail` with those skips, on the same change over M175
   before that rebase: lines 97.42%, regions 96.05%
   (`provisional`: a 4-core cloud container). CI's coverage and mutants runs on the PR are
   the gate.
