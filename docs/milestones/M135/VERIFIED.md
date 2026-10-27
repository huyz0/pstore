# M135 verification

The spec was reviewed in one round; its findings are recorded in the [spec](SPEC.md).

How to run the tests: `cargo test -p pstore-query` and
`cargo test -p pstore-server --all-features --test it arrays::`.

Red runs, against the parent's code with the new tests in place:
- Criteria 1, 4 and 6 did not compile: `Predicate::AnyCmp` did not exist.
- Criteria 2 and 5 failed: the parent answered `400`, "unknown operator AnyLt".
- Criterion 3 failed on the parent's "unknown operator AnyLt" refusal text.

The mutations were applied one at a time by a script and then reverted. No `cargo mutants`
run is claimed for them.

1. `any_comparisons_match_the_reference`: green after. Killed: "a scalar attribute is
   compared as a one-element array", "`all` instead of `any`", "an empty array is true",
   "`Lt` evaluated as `Lte`", "`Gte` evaluated as `Gt`", and "`AnyCmp` prunes on the scalar
   zone" (its `could_admit` assertion).
2. `any_comparisons_find_an_element_by_order`: green after, folded and not. Killed: each of
   the four parser arms given its neighbour's operator (`AnyLt` as `Lte`, `AnyLte` as `Lt`,
   `AnyGt` as `Gte`, `AnyGte` as `Gt`).
3. `what_an_array_cannot_hold_is_refused`: green after; red on the parent as above.
4. `every_predicate_round_trips` and `every_truncation_and_every_unknown_tag_is_none`:
   green after. Killed: "operator code written as `Eq`" (both), "tag 12 accepts `Eq`" and
   "tag 12 accepts an array literal" (the second).
5. `delete_by_filter_takes_any_comparisons`: green after, folded and not. It also killed
   "`AnyGte` as `Gt`".
6. `mixed_ints_and_floats_prune_soundly_with_zone_maps`, with `predicates()` extended:
   green after. Killed: "`AnyCmp` prunes on the scalar zone".
