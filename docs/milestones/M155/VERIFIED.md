# M155 verification

The spec was reviewed in two rounds before any code.
- **Round 1** had four majors. The engine does change, for an unknown text field. An index
  whose rows hold only further fields made a string leg a `500`. The mapping could name the
  wrong field. Criterion 2's expected scores ignored the per-leg limit.
- **Round 2 passed.** Its minors were applied: the refusal names the first missing field by
  name, and four pinned query-layer tests change, not five.

Three amendments came after the code, each found by running it:
- **Criterion 2's limits.** `Sum`'s legs are whole, not cut at their limit (M9g.2), so its
  twins are unlimited. Only `Max`'s are cut at `top_k`. The first draft of the test cut both,
  and `Sum` disagreed.
- **BM25, not RRF.** A query naming no fusion is RRF, whose scores are ranks, so criteria 1
  and 2 name `Sum` with weight 1 on every single-leg query. Comparing RRF scores would have
  passed with any ranking that agreed in order.
- **Ties.** The first rows repeated with a period of 15, and the test's own near-tie check
  refused them. Each value is now padded with words no query names, by different moduli.

Also, criterion 7 named one pinned refusal of `full_text_search` on another attribute. The
workspace run found a second, in `analyzer.rs`, reversed the same way. The two-leg refusal
case uses `zzz` rather than `nosuch`, so that a refusal naming the first queried field would
name `title`.

How to run the tests: `cargo test -p pstore-server --test it text_fields::`.

Red runs: against the parent, with the test file alone, all six tests failed. Five failed at
the first write: "schema: \"tags\": full_text_search is on the text field `text` only". The
door test failed inside its loop of refusals, which the parent answered with its own message.
Each test's power to fail rests on the named mutations below. They were applied one at a time
by a script, `text_fields::` run, and then reverted. All 13 were observed red. No
`cargo mutants` run is claimed for them.

1. `one_further_field_ranks_as_its_own_index`: green after. Killed:
   - "an object leg is read as a leg over `text`";
   - "a further field is declared with the default `FullText`";
   - "`Declared.texts` is not passed".
2. `several_fields_fuse_as_their_twins`: green after. Killed: "text legs are reordered by
   field".
3. `what_a_further_field_cannot_be_is_refused_at_the_door`: green after. Killed: "a further
   field's options skip the parser's checks", "a further field's document value is not
   checked".
4. `further_fields_are_fixed_when_the_index_is_created`: green after. It has no mutation of
   its own, as the spec says: the conflict check is M152's. "`Declared.texts` is not passed"
   breaks it too.
5. `the_query_syntax_and_its_refusals`: green after. Killed:
   - "`query_error` leaves `UnknownTextField` a generic query error";
   - "the refusal names the first text leg's field";
   - "a primary leg is never refused as unknown";
   - "highlight accepts a further-field leg";
   - "an object leg's unknown key is ignored".

   Its `[]` refusal is a guard, green on the parent.
6. `the_index_reports_its_further_fields`: green after. Killed: "`text_fields` is always
   written", "`text_fields` reports the defaults".
7. The two reversed cases (`sketch.rs`, `analyzer.rs`) are green after. The pinned
   query-layer tests now assert `UnknownTextField` and the field it names, and are green after:
   - pstore-query `text_field::a_query_naming_the_wrong_text_field_is_refused`;
   - pstore-query `multi_text.rs`'s refusal of `d`;
   - pstore-query `several_fields::what_several_fields_cannot_mean_is_refused`;
   - pstore-engine `multi_text::an_unknown_field_beside_a_known_one_is_refused`.

The whole workspace's tests pass, except the two IPv6 `peer_http::response_tests`, which fail
in this container on the parent too. `analyzer.rs`'s case failed in the first run and was
reversed as described above, and those suites were run again.

Code review round 1 passed with no blocking or major findings. Its minors:
- `query_error`'s new arm had no unit case beside a `multi` leg. One was added to
  `query_error_tests`, a `MultiFde` leg with `UnknownTextField("zzz")`. The arm's order cannot
  matter, since no other arm matches that variant, but removing the arm turns it red, as
  "`query_error` leaves `UnknownTextField` a generic query error" already showed over HTTP.
- The four malformed-leg cases check the status, the code and that the message holds "text".
  A regression refusing them through a generic deserialization error would still pass. The
  "unknown key is ignored" mutation guards the one that matters most.
