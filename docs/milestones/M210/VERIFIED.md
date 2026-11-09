# M210 verification

How to run the tests: `cargo test -p pstore-query --lib aggregate` and
`cargo test -p pstore-server --test it -- group_each::`.

Red runs: with the aggregator's new types in place but every entry still keyed as a plain
value, the three new `pstore-query` unit tests failed (3 failed, 4 passed). With the parent's
parse (object entries and `["Count"]` refused), all seven `group_each::` tests failed.

Existing tests changed, as criterion 8 states: `aggregate.rs`'s refusal list lost
`{"aggregate_by": {"n": ["Count"]}}`, now accepted;
`group_each::unary_count_counts_rows_and_takes_the_fast_path` asserts the acceptance.

Mutations were applied one at a time by a script and reverted, against the unit tests and
`group_each::`. 9 of 9 were killed. No `cargo mutants` run is claimed for them.
- elements not deduplicated; a scalar given no group; an absent value or empty array given
  no group; the element written into the last key slot rather than its own; `kept` taken
  from the last element only; an array keyed whole rather than by element; a second
  `ForEachUnique` accepted; unary `Count` counting an attribute no row has; a named group
  sharing a plain entry's attribute accepted.

1. `each_unique_element_is_one_group_through_folded_and_unfolded_rows`: green after, with and
   without a filter, and `meta.unfolded_hits` equal to the unfolded rows admitted.
2. `an_element_twice_in_one_row_counts_the_row_once` and the unit test
   `an_element_twice_in_one_array_counts_the_row_once`: green after. Killed "no dedupe".
3. `an_exploded_key_combines_with_a_plain_one_in_key_order` and
   `exploded_groups_combine_with_plain_keys_and_keep_the_smallest_exactly`: green after.
   Killed "element written into the last slot".
4. `the_smallest_elements_are_exact_across_segments` (top_k 1 and 2 over two folded
   segments and unfolded rows): green after; the unit test also kills "kept from the last".
5. `a_multi_query_and_facets_take_named_groups`: green after.
6. `unary_count_counts_rows_and_takes_the_fast_path`: green after, 1 blob read unfiltered on
   a second server. Killed "unary Count counts nothing".
7. `what_a_named_group_cannot_mean_is_refused_before_any_blob_request`: green after, alone
   and inside `queries`, with no blob request. Killed both refusal mutations.
8. `cargo test -p pstore-query -p pstore-engine -p pstore-server --no-fail-fast` passed but
   for one test: `peers::a_dropped_membership_leaves_the_list_too` failed once under the
   parallel run (a gossip UDP port) and passed alone; it touches no aggregation. The two IPv6
   peer tests were skipped, as this container has no IPv6.

Gates, in a cloud container: `cargo fmt --all --check` and `cargo clippy -p pstore-query
-p pstore-server -p pstore-engine --all-targets -- -D warnings` passed. Coverage and
`scripts/mutants.sh`: NOT-RUN here; CI runs both on the pull request.
