# M163 verification

The spec was reviewed in one round, and the code in one (fail on one major, fixed: an
unneeded edit to an M146 test, reverted); both are recorded in the [spec](SPEC.md).

How to run the tests: `cargo test -p pstore-server --test it -- relevance_offset:: order::`.

Red runs: every `relevance_offset::` test but the refusals failed on the parent, which
refused `offset` without `rank_by`; the refusal test failed on the parent too, by the
exact-rerank panic in `clamp` (`core/src/cmp.rs`). Its other cases pass on the parent, so
their red is the mutations below. Criterion 6 is a guard.

Existing tests changed, as criterion 6 states:
- `order::rank_by_refuses_what_it_cannot_mean` asserted `{"vector": ..., "offset": 1}` is
  refused; that is the answer this milestone changes. The case is now `offset` beside
  `diversify`, still refused, and the new answer is asserted by `relevance_offset::`.

The mutations were applied one at a time by a script and then reverted, against the
server's `relevance_offset::` tests. All 12 were killed. No `cargo mutants` run is claimed
for them. "The window's default from `top_k`" is the same edit as "the window checked
against `top_k`": at these sizes both defaults are 100, so only the check kills it.
"The skip before the cut" is equivalent without a window: only the boosted and grouped
cases kill it. Peers and `as_of` receive the page end by construction; no peered query is
tested.

1. `a_page_is_the_slice_of_the_ranking_at_its_depth` (dense, text, hybrid RRF, text
   `sum`, exact; offsets 0, 1, 3, 7): green after. Killed by "the engine ranked at
   `top_k`", "the legs cut at `top_k`" and "the skip not done".
2. `exact_pages_concatenate_to_one_ranking`: green after. Killed by "the engine ranked at
   `top_k`" and "the legs cut at `top_k`".
3. `a_boosted_or_group_limited_page_is_a_slice_of_its_ranking`: green after. Killed by
   "the skip before the cut" and "the group cut at `top_k`".
4. `unfolded_hits_count_the_returned_rows`: green after. Killed by "`unfolded_hits` from
   the engine's hits with an offset".
5. `what_cannot_mean_a_page_is_refused_before_any_blob_request`: green after, every
   refusal without a blob request. Killed by "the boost window checked against `top_k`",
   "the group window checked against `top_k`", "the bound `>=`", "the bound past
   10,001", "`offset` accepted beside `diversify`" and "the exact-rerank depth check
   removed".
6. `cargo test -p pstore-server --test it` passed (462). The server's library tests
   passed but for the two IPv6 tests this container cannot run.
7. `facets_beside_a_page_answer_as_beside_the_first`: green after; a guard, by
   construction.

Gates, in a cloud container:
- `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings`
  passed.
- ⚠️ Coverage: OBSERVED-NOT locally (see [M136](../M136/VERIFIED.md)). The floor is CI's
  run on push.
- NOT-RUN here: `scripts/mutants.sh` (`cargo-mutants` is not installed in this container).
