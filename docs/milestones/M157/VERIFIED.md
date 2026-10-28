# M157 verification

The spec was reviewed in two rounds before any code.
- **Round 1** had three majors and three minors. The majors:
  - a named mutation ("`open_part` counts every field's terms against each field") that the
    data could not kill, since nothing reads a `df` no leg asks for. It was replaced;
  - the door had no rule for a part at 1 to 6 over several fields;
  - the coordinator never checked which fields a peer's phase-1 reply covers.

  The minors: the engine's half of the type change, an undeclared field costing a failed
  share (now `shareable_fields`), and a scan with no `stats`. All six were fixed.
- **Round 2 passed.** Its one minor, that criterion 2's one-field query must read a further
  field for its mutation to show, was applied.

Two amendments after the code, each a test the spec did not list among those it reverses:
- M151's `two_fields_are_refused_before_any_request_on_every_path` (`multi_text.rs`) also
  asserted that `part` refuses legs over two fields. The Delta lifts that refusal for
  `open_part` and `part` together, so that half is removed. Its `query_rows_split` half, a
  single-`FullText` entry point, is unchanged and still refuses before any request.
- The reversed M153 test, `a_query_over_several_text_fields_is_not_split`, keeps its name, as
  M156's did under M154, because M153's ledger cites it. Its several-field half now asserts
  an open and a scan call; its primary half is unchanged.

The test harnesses changed with the types: `split.rs`'s `Three` peers, and
`a_share_sums_only_what_its_legs_and_head_ask_for`, read a `FieldStats` where they read a
`Stats`. A dense-only part's statistics are now no entry at all, where they were an empty
`Stats`.

How to run the tests:
- `cargo test -p pstore-engine --test it split::`
- `cargo test -p pstore-server --test it peers::`

Red runs, against the parent with the tests alone:
- `a_query_over_several_text_fields_is_split_as_unsplit` failed: "rrf over text and title: no
  open call". Its unsplit and split answers were equal, and not empty.
- M153's reversed `a_query_over_several_text_fields_is_not_split` failed: "a query over text
  and title was not split".
- `a_query_over_one_further_field_carries_its_terms_alone` does not compile on the parent:
  `Part.terms` is a list there.
- `a_query_over_several_text_fields_is_split_as_one_server_answers` failed: "text and title
  at 0 was not split".
- `a_part_over_several_fields_is_its_own_protocol` failed: no part reached the old peer, so
  `pstore_peer_parts_failed` did not grow.
- `a_phase_one_reply_short_of_a_field_is_a_failed_share` failed the same way.
- `a_part_over_several_fields_is_judged_at_the_door` failed: the parent answered "7 with
  terms" with `409 protocol_mismatch`, not `400`.

Each test's power to fail rests on the named mutations below. They were applied one at a
time by a script, the test run, and then reverted. All 26 were observed red. No
`cargo mutants` run is claimed for them.

1. `a_query_over_several_text_fields_is_split_as_unsplit`: green after. Killed:
   - "`run` sums every share's statistics into the first field";
   - "`open_part` summarises every field from the first field's index";
   - "a field's terms are analysed under the primary's analyzer";
   - "`one_text_field` is kept".
2. `a_query_over_one_further_field_carries_its_terms_alone`, and `split.rs`'s M154 tests
   unchanged: green after. Killed: "a one-field query's `Part.terms` holds an entry for the
   primary too".
3. `a_query_over_several_text_fields_is_split_as_one_server_answers`: green after. Killed:
   "`WirePart` drops `field_terms`"; "phase 1 answers only the first field's statistics";
   "phase 2 is sent only the first field's statistics".
4. `a_part_over_several_fields_is_its_own_protocol`: green after. Killed: "a several-field
   part is sent at M154's number"; "a one-field part is sent with `field_terms`";
   "`shareable_fields` admits an undeclared field".
   `a_phase_one_reply_short_of_a_field_is_a_failed_share`: green after. Killed: "the
   coordinator does not check a reply's `field_stats` keys"; "the coordinator does not check
   a one-field reply for `stats`".
5. `a_part_over_several_fields_is_judged_at_the_door`: green after. Each refusal removed, one
   at a time, was killed:
   - fewer than two `field_terms`; more than nine;
   - `terms` at 7 or 8;
   - `field_terms` that are not the legs' fields;
   - `field_terms` at 1 to 6; several fields at 1 to 6;
   - `texts` at 7 and 8;
   - the sum coupling ignoring 8;
   - a scan's fields axis unchecked; a scan's sum axis unchecked;
   - a per-field scan carrying `stats`; a one-field scan carrying `field_stats`; a one-field
     scan without `stats`.

Code review round 1 passed on the code. Its major was the workspace line below, staged
before the run it reports had finished; it was committed only after that run. Its one
minor is recorded rather than fixed: criterion 1 asks for `df`s that differ by field by at
least a factor of two, and `title`'s "run" (one row in 5) and `tags`'s (one in 8) differ by
1.6. Every pair still differs, so a mix gives a different `df`, and the mixing mutations
were observed red.

The whole workspace's tests pass, except the two IPv6 `peer_http::response_tests`, which fail
in this container on the parent too.
