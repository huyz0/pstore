# M151 verification

The spec was reviewed in two rounds before any code.
- **Round 1 was blocking.** An all-empty field changed the statistics, and `decode` could not
  make two of its checks. The decode cache would have churned, and the analyzer was not
  per-field. All of these were fixed.
- **Round 2 passed.** Its major, deciding the missing-field refusal from the footers rather
  than from the shares' statistics, was applied, as were its minors.
- Criterion 5 was reworded during implementation to what its test asserts.

How to run the tests:
- `cargo test -p pstore-format --test it multi_text::`
- `cargo test -p pstore-query --test it multi_text:: text_field::`

Red runs: against the parent, criteria 1 to 5 do not compile, because `build_fields`,
`Fields`, `Dictionaries`, `for_field` and `try_build_all_fields` were absent. That only shows
the API is new. So each test's power to fail rests on the named mutations below, each
observed red. The mutations were applied one at a time by a script and then reverted. No
`cargo mutants` run is claimed for them.

Two mutations first survived: "decode accepts `N != n`" and "decode accepts a gap". The
gap fixture moved a middle field, which also overlapped the next one, and no fixture
reached the count check on its own. The test now moves the last field, and adds a valid
two-field container whose header claims three. Both mutations were then killed.

1. `one_field_builds_as_it_always_has` (format) and
   `one_text_field_writes_the_segment_it_always_wrote` (query): green after. Killed: "a
   one-field build writes a container".
2. `each_carried_field_is_its_own_one_field_build`, `what_a_field_list_cannot_mean_is_refused`
   and `a_dictionary_is_a_slice_of_the_buffer_not_a_copy`: green after. Killed: "postings
   starts are not shifted" (in the build), "a field with no text is carried".
3. `a_field_of_several_answers_as_its_own_segment_would`: green after. Killed:
   - "the fieldnorms are not offset by field";
   - "postings starts are not shifted" (in the read);
   - "`for_field` opens field 0";
   - "the statistics are field 0's";
   - "a segment without the field is `UnknownField`".
4. `a_malformed_container_decodes_as_nothing`, `what_one_field_per_query_cannot_answer_is_refused`
   and `a_share_without_the_field_adds_nothing_and_refuses_nothing`: green after. Killed:
   - "decode accepts `N != n`";
   - "decode accepts a gap";
   - "text legs over different fields are accepted", once each for `run` and `open_part`;
   - "`from_segment` opens field 0 of several";
   - "a field no segment carries answers nothing", also killed by M6c.2's
     `a_query_naming_the_wrong_text_field_is_refused`;
   - "`open_part` refuses a field its segments lack".
5. `a_field_of_several_costs_what_one_field_costs`: green after. Killed: "the fieldnorms are
   not offset by field", "postings starts are not shifted" (in the read), and "`for_field`
   opens field 0".

Code review round 1 was blocking. Criterion 4 was only partly tested: nothing reached
`for_field`'s three section checks, the phased-share path, or the one-call `part`. It also
found that a field whose text analyzes to no token was carried. That broke criterion 3's
equivalence with a one-field segment, which carries no text for it. The fixes:
- **The carry rule:** a field is now carried when it has postings, and the spec says so.
- **The refusal:** it moved from `scan_part`, which only runs an `OpenPart` the refusing paths
  built, to `part`, before its open round.
- **The tests:** they were added below, and each was observed red under its mutation.

- `text_with_no_token_is_not_carried`: green after. Killed: "carried by `has_text`, not by
  postings".
- `sections_that_disagree_with_the_sidecar_are_refused`: green after. Killed: "postings past
  the spans accepted", "the fieldnorms width unchecked", "a repeated name accepted".
- `two_fields_are_refused_before_any_request_on_every_path`, through `query_rows_split` with
  a phased share and through `part`, with no request made: green after. Killed: "`part`
  refuses after opening", "text legs over different fields are accepted" (`run`).
