# M182 verification

The spec was reviewed in three rounds before any code. Round 1 raised three majors (the
leg's tie and fusion order; the statistics rule; mutations no test could catch), round 2 one
(an `f64` score on the `f32` wire), round 3 passed with one minor (an `f32` idf can round to
0), applied. One correction was made while implementing, in Delta 2: a segment carries a
field's dictionary when some row has **text** under it (a non-empty string, M150), not a
token, as the segment builder decides; the oracle counts by that rule.

How to run the tests: `cargo test -p pstore-query --test it expressions::`,
`cargo test -p pstore-engine --test it order::an_expression_query_takes_three_rounds` and
`cargo test -p pstore-server --test it scoring::`

Red runs. `pstore_query::rank` was first a stub returning no rows: `expressions_match_the_oracle`,
`a_bm25_clause_is_the_text_leg` and `zero_scores_are_dropped` failed on an empty answer.
`scoring_expressions_over_http` was run against the parent's `pstore-server/src/lib.rs` and
failed on `rank_by` refusing the vendor's nested example (400). The depth test was written
after the engine call, so it was seen red by its two mutations (5 below), not by a stub.

Each named mutation was applied by a script, the tests above run, and the mutation reverted;
the tree was compared with its state before the run. No `cargo mutants` run is claimed for
them. All were killed.

1. `expressions_match_the_oracle`: green after, 300 seeded trees, the kept set exact and
   scores within 1e-4 relative. Killed: `Max` as `Sum`; `Product` ignoring its weight; a
   filter leaf scoring 0; `tf` counted once per term; deleted rows scored; the length taken
   from the raw tokens (stopwords kept) rather than the terms. The first oracle run failed on
   an exact reference tie that the answer ordered by its own `f64` scores a bit apart, so
   `rank` returns the `f64` it ordered by and the test checks id order within the answer's
   own ties; the narrowing to `f32` happens at the wire.
2. `a_bm25_clause_is_the_text_leg`: green after, 20 queries per field with and without
   `last_as_prefix`, sealed and with one segment's rows passed as unfolded. Killed: the
   prefix's 1.0 added per matching term; the unfolded rows' statistics leaving out rows that
   lack the field.
3. `zero_scores_are_dropped`: green after. Killed: `> 0` as `>= 0`; a tie broken by row
   rather than id (row ids are permuted so that row order is not id order).
4. `scoring_expressions_over_http`: green after. Killed: the unfolded rows left out of the
   statistics; an order pair read as an expression; a negative weight accepted; `Product`'s
   reversed form refused. `expressions_out_of_bounds_are_refused` covers the limits in the
   query crate and was not seen red.
5. `an_expression_query_takes_three_rounds`: green after, 3 rounds and exactly
   `2 + 3 × segments` requests for 1 and 8 segments, folded and with an unfolded row. Killed:
   posting lists fetched for BM25 (4 rounds); the dictionaries fetched in a round of their own
   (4 rounds).
6. Gates: `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features --
   -D warnings` pass. `cargo test --no-fail-fast` over pstore-format, -index, -query, -engine
   and -server reported no failure but the two IPv6 `peer_http::response_tests`, NOT-RUN in
   this container (no IPv6), as on every parent. `cargo deny check`, coverage and the mutation
   sweep are NOT-RUN here; they are CI's on the PR.

Code review, round 1: pass, with three minors. Fixed: a weight such as `1e-50` narrowed to
an `f32` 0 and was accepted, where Delta 6 refuses it; the range is now checked on the
request's number, and `scoring_expressions_over_http` refuses `1e-50` (seen red before the
fix). Kept: a filter on an attribute named `Sum`, `Max` or `Product` reads as that operator
inside `rank_by` (stated in `docs/deploy.md`); `expressions_out_of_bounds_are_refused` was
not seen red, as item 4 says.

CI's mutation shards on the PR missed three mutants, each now killed, by hand as above:
`rank`'s `s > 0.0` before an unfolded row's copy (redundant with `Top::offer`'s own test, so
`>=` changed nothing: the two now share `Top::wants`, and its mutants fail
`zero_scores_are_dropped` and `top_refuses_zero_and_an_empty_cap`); `Top::offer`'s
`>=` as `<` (pruning on every row, the same answer at a sort per row: killed by
`top_prunes_when_full_and_never_holds_more`); and the BM25 params object's arm deleted (no
test sent a valid one: `scoring_expressions_over_http` now sends `{"last_as_prefix": true}`
and `{}`, and the deleted arm fails it).
