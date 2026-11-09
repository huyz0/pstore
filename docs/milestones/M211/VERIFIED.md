# M211 verification

How to run the tests: `cargo test -p pstore-query --lib aggregate` and
`cargo test -p pstore-server --test it -- group_expanded::`.

Red runs: with `Min`, `Max` and `Avg` folding nothing and every bucket the absent key, the two
new unit tests failed (2 failed, 7 passed). With the parent's parse (the three kinds and
`Bucket` refused), all five `group_expanded::` tests failed.

Existing tests changed, as criterion 7 states: `aggregate.rs`'s refusal case
`["Avg", "price"]` became `["Median", "price"]`, refused by the same message; `Avg` is now
accepted and criterion 1 asserts it.

Mutations were applied one at a time by a script and reverted, against the unit tests,
`group_expanded::` and `group_each::`. 8 of 8 were killed. No `cargo mutants` run is claimed.
- `Min` reading the largest; a merge dropping the other side's extremes; `Avg` divided by
  every row offered; an integer bucket and a float bucket truncated rather than floored; a
  uint64 bucket computed in floats; a width of 0 accepted; a bucket's name repeated accepted.

1. `min_max_and_avg_equal_a_reference_through_folded_and_unfolded_rows`: green after,
   ungrouped and by `color`, filtered and not, and `null` for a group holding only strings.
   Killed "Avg divided by rows" and "merge drops extremes".
2. `min_max_and_avg_are_typed_as_stated`: green after. Killed "Min reads max".
3. `buckets_are_floored_lower_bounds_with_non_numbers_last` and
   `a_bucket_is_its_lower_bound_floored`: green after, widths 10 and 2.5, negatives below zero.
   Killed both truncation mutations and "uint bucket in floats".
4. `a_bucket_combines_with_plain_and_exploded_keys_and_is_bounded`: green after, beside
   `color` and a `ForEachUnique`, and at `top_k` 1 and 2 over two segments.
5. `a_multi_query_and_facets_take_expanded_grouping`: green after.
6. `what_expanded_grouping_cannot_mean_is_refused_before_any_blob_request`: green after,
   alone and inside `queries`, with no blob request. Killed both refusal mutations.
7. `cargo test -p pstore-query -p pstore-engine -p pstore-server --no-fail-fast` passed (the
   server's `it` suite: 658 passed, 1 ignored), skipping the two IPv6 peer tests this container cannot run.

Gates, in a cloud container: `cargo fmt --all --check` and `cargo clippy -p pstore-query
-p pstore-server -p pstore-engine --all-targets -- -D warnings` passed. Coverage and
`scripts/mutants.sh`: NOT-RUN here; CI runs both on the pull request.

Code review round 1 (pass, two minors, both taken): a number whose integer bucket bound
falls below `i64::MIN` was put in the `null` group; it is now the float nearest its bound,
asserted in `a_bucket_is_its_lower_bound_floored` (red with the old fallback, green after).
The migration notes now say a width of `10.0` is a float width.

CI's `mutants-shard (0)` on the pull request found four mutants of `Width`'s `PartialEq`
missed (each arm deleted, each `==` turned `!=`): nothing compared two widths. The unit test
`widths_are_equal_only_of_one_kind_and_value` was added; it fails under each of the four and
passes on the code, run one at a time by script here.
`mutants-shard (3)` then timed out on `+=` turned `-=` and `*=` for a uint64 in
`Acc::number`: no test averaged a uint64, so the surviving mutant ran the whole suite past
the timeout. `min_max_and_avg_are_typed_as_stated` now averages uint64s beside an int; it
fails under both mutants and passes on the code, run by script here.
`mutants-shard (11)` missed the float width's guard made `true` and its `&&` made `||`: no
test sent a non-positive float width. The refusal cases in `group_expanded.rs` now send
-2.5 and 0.0; both mutants fail them, run by script here. A non-finite width cannot be
written in JSON, so `is_finite` guards only a value no client can send.
