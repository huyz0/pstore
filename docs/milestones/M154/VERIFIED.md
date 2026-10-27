# M154 verification

The spec was reviewed in two rounds before any code.
- **Round 1** had one major and four minors. The major: criterion 1's fourth query, a filter
  over the primary, could use words both analyzers read alike, so a peer binding it under the
  queried field's analyzer would pass. The minors: the sum coupling at 5 and 6 and the scan
  mismatches in the door, both peer bind sites in the mutation list, the scan's protocol
  under criterion 4, and a cap on `texts` at the door.
- **Round 2 passed.** Its one minor, a scan at 3 of a part opened at 6, was added.

One amendment after the code: criterion 4 named `{"text": {"field", "query"}}`, which the
query syntax does not accept. A leg object goes in an array, `{"text": [{"field", "query"}]}`
(M155), and the test uses that.

How to run the tests:
- `cargo test -p pstore-engine --test it split::`
- `cargo test -p pstore-server --test it peers::a_part_with peers::a_query_over_a_further`

Red runs, against the parent with the tests alone:
- `a_query_over_a_further_text_field_is_split_as_unsplit` failed: "rrf: no open call".
- `a_token_filter_over_a_further_field_is_split_as_unsplit` failed: "vector-only
  Tokens("title", Any, "Running", None): no part call".
- M156's reversed `a_token_filter_over_a_further_field_is_not_split` failed the same way, on
  its further-field half.
- `a_query_over_a_further_text_field_is_split_as_one_server_answers` failed: "over title at 0
  was not split".
- `a_part_with_further_fields_is_its_own_protocol` failed on "over title": no part reached the
  old peer.
- `a_part_with_further_fields_is_judged_at_the_door` failed: the parent answered "open at 5
  without texts" with `409 protocol_mismatch`, not `400`.
- M153's `a_query_over_several_text_fields_is_not_split` (criterion 3) passed, a guard.

Each test's power to fail rests on the named mutations below. They were applied one at a
time by a script, the test run, and then reverted. All 18 were observed red. No
`cargo mutants` run is claimed for them.

1. `a_query_over_a_further_text_field_is_split_as_unsplit`: green after. Killed:
   - "the peer scores under `part.fts`";
   - "the coordinator analyses terms under the primary's analyzer";
   - "`texts` is never filled";
   - "a token predicate over the primary is bound under the queried field's analyzer". It
     **survived the first run**: the fourth query ranked "running stories", whose top rows all
     held "walking" in `text`, so the wider set the wrong bind admits never reached the top
     10. The query is now "running twice", a word only the rows holding "walk" have in their
     title, and the mutation was observed red.
2. `a_token_filter_over_a_further_field_is_split_as_unsplit`: green after. Killed:
   - "`Engine::part` binds the filter with `bound`";
   - "`Engine::scan_part` binds the filter with `bound`";
   - "`texts` is filled only for a text leg over a further field".
3. `a_query_over_several_text_fields_is_not_split`: unchanged, green before and after.
4. `a_query_over_a_further_text_field_is_split_as_one_server_answers`: green after. Killed:
   - "the coordinator sends no `texts`";
   - "the peer reads no `texts`";
   - "a scan of a part with `texts` is sent at today's number".
5. `a_part_with_further_fields_is_its_own_protocol`: green after. Killed: "a part with `texts`
   is sent at today's number", "`texts` is always filled".
6. `a_part_with_further_fields_is_judged_at_the_door`: green after. Each refusal removed, one
   at a time, was killed:
   - `texts` outside 4 to 6;
   - more than 8 `texts`;
   - `texts` that do not decode;
   - the sum coupling ignoring 6;
   - a scan's `texts` axis unchecked;
   - a scan's sum axis unchecked.

The reversed tests, as the spec's "Reversed criteria" says:
- M152's `a_query_over_a_further_text_field_is_not_split` is replaced by criterion 1's test,
  over a field that differs from the primary in its analyzer as well as in `k1` and `b`.
- M156's `a_token_filter_over_a_further_field_is_not_split` keeps its primary half unchanged,
  and asserts that its further-field half is split and answers exactly as unsplit.

Code review round 1 found no fault in the code. Its one major: the evidence files were not
staged. They are staged in this commit, so `scripts/check-verified.py` judges the tree as it
is committed. Its minors:
- M152's ledger names the deleted test without backticks, under a banner naming the test
  that replaced it, so the checker does not try to resolve a test that no longer exists.
- M156's reversed test keeps its name, which M156's ledger cites. Its `split` flag is now
  always true, so its "not split" branch is dead. It is left as the reversal the spec names
  rather than restructured.

The whole workspace's tests pass, except the two IPv6 `peer_http::response_tests`, which fail
in this container on the parent too.
