# M146 verification

The spec was reviewed in one round, and the code in one, pass with two minors, both
taken; both are recorded in the [spec](SPEC.md).

How to run the tests:
- unit: `cargo test -p pstore-server --lib limit_per::`;
- server: `cargo test -p pstore-server --test it -- limit_per:: boost:: diversify:: composition::`.

Red runs: criterion 1's tests failed to compile before `LimitPer` and `per_group` existed,
which the spec counts as red. Criteria 2, 3 and 5 failed on the parent, which refused the
unknown field (M126). Criterion 4's test passed on the parent for that reason, so its red
is the mutations below; its cases no mutation names (not an object, an unknown key, a missing
`attributes` or `limit`, no attribute, a non-string name, a window past 1,000 or not whole,
`top_k` past 1,000, beside `aggregate_by`) are guards. Criterion 6 is a guard.

The mutations were applied one at a time by a script and then reverted, each run against
the unit tests, the server's `limit_per::` tests, or both, as listed. All 22 runs were
killed, and one added after code review (below), 23 in all. No `cargo mutants` run is
claimed for them. "The walk does not stop at `top_k`" was
run against the unit tests only: in the server, the final truncation to `top_k` makes it
equivalent.

1. `the_cut_equals_a_reference_over_random_rows` and
   `one_and_one_point_zero_are_one_group_and_absent_is_a_group`: green after. Killed by
   "the count compared with `<=`", "the walk does not stop at `top_k`", "groups by the
   first attribute only" and "`1.0` a group apart from `1`" (a mutation of `Key::of`).
2. `the_limit_cuts_the_windows_ranking_per_group`: green after; it asserts the plain
   ranking's first `top_k` rows hold more than `m` of one group at each `m`, and checks a
   query without `g` in `include_attributes`. Killed by "`<=`", "`1.0` apart" and "groups
   read from the selected attributes".
3. `with_a_boost_the_cut_is_of_the_boosted_order` and
   `a_hybrid_query_is_cut_and_unfolded_hits_count_the_kept_rows`: green after. Killed by
   "the cut applied before the boost", "`unfolded_hits` taken from the engine's hits" and,
   added after code review with a case where only `limit_per` names the window, "a window
   only `limit_per` names is ignored by the boost".
4. `what_cannot_mean_a_per_group_limit_is_refused_before_any_blob_request`, which checks
   no refusal made a blob request, and the unit
   `what_cannot_mean_a_per_group_limit_is_refused` and
   `the_window_defaults_as_a_boosts_and_is_bounded`: green after. Killed by "`limit` 0
   accepted", "9 attributes accepted", "a repeated name accepted", "beside `diversify`
   accepted", "beside `rank_by` accepted", "a window below `top_k` accepted" and "a
   boost's other window accepted".
5. `a_per_group_limit_costs_a_boosts_window`: green after; the same depth and request
   count as a boost of the same window, each on a cold server, both greater than 0.
6. `cargo test -p pstore-server --test it` passed (440), `boost::`, `diversify::` and
   `composition::` among them, unedited; `pstore-query`'s tests passed (35 library, 59
   integration). The server's library tests passed but for the two IPv6 tests this
   container cannot run.

Gates, in a cloud container:
- `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings`
  passed.
- ⚠️ Coverage: OBSERVED-NOT locally (see [M136](../M136/VERIFIED.md)). The floor is CI's
  run on push.
- NOT-RUN here: `scripts/mutants.sh` (`cargo-mutants` is not installed in this container).
