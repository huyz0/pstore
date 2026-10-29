# M164 verification

The spec was reviewed in one round (block on two majors, amended), and the code in one
(fail on one major, this file missing, fixed); both are recorded in the [spec](SPEC.md).

How to run the tests: `cargo test -p pstore-server --test it -- multi_aggregate:: snapshot::`.

Red runs: before the handler changed, all five `multi_aggregate::` tests and both new
`snapshot::` tests failed (6 failed, run on the tests alone): the parent refused
`aggregate_by` and `facets` inside `queries`. Of criterion 6's cases, every one but
`group_by` without `aggregate_by` failed on the parent by its message ("inside a
multi-query is not offered"); that one is a regression guard.

Existing tests changed, as criterion 7 states:
- `aggregate::` refusal list: `{"queries": [count()]}` removed; it is now accepted, as
  `multi_aggregate::each_entry_is_the_sub_querys_own_answer` and
  `the_new_fields_appear_only_when_asked_for` assert.
- `facets::` refusal list: `{"queries": [{"vector": Q, "facets": ...}]}` removed; the same
  two tests assert its acceptance.
- `snapshot.rs`'s `three` doc comment no longer says a multi-query offers no
  `aggregate_by`; no assertion changed.

The mutations were applied one at a time by a script and then reverted, against the
`multi_aggregate::`, `snapshot::`, `facets::`, `aggregate::` and `rerank::` tests. 8 of 9
were killed. No `cargo mutants` run is claimed for them.
- Survived, equivalent: "`facets_epochs` from the ranked leg's epoch". Every faceted pair
  now runs under one pin, the request's or its own, or reads one `as_of` epoch, so the two
  epochs are equal on every path; the spec review predicted this.
- Not applied: "the outer limit applied to facets" and "the aggregation run at another
  sub-query's `top_k`" have no code path to mutate (nothing passes one sub-query's window
  to another); criterion 2's test asserts the full counts as a guard.

1. `each_entry_is_the_sub_querys_own_answer`: green after. Killed "aggregations put in
   aggregation_groups".
2. `no_rank_window_truncates_an_aggregation`: green after (30 and 40 counted at `top_k`
   1, 4 groups at `top_k` 4, facets' 4 groups under an outer `limit` 1). Also kills the
   field swap.
3. `snapshot::aggregations_and_facets_share_the_legs_snapshot`: green after. Killed "the
   request run unpinned".
4. `the_new_fields_appear_only_when_asked_for`: green after. Killed "aggregations
   serialized when `None`" and "facets serialized when `None`".
5. `aggregations_add_no_depth`: green after, depth measured above 0. Killed "facets run
   after the ranked legs": the depth mutation is not equivalent here.
6. `what_cannot_mean_is_refused_before_any_blob_request`: green after, every refusal
   without a blob request. Killed "the rerank refusal removed" and "facets derived after
   planning" (the refusal then names the leg, not `facets`).
7. `cargo test -p pstore-server --test it` passed (508). The server's library tests
   passed but for the two IPv6 peer tests this container cannot run.
8. `snapshot::an_unpinned_multi_query_pins_each_faceted_pair`: green after. Killed "the
   per-pair pin removed", rerun after the review's added assertion.

Gates, in a cloud container:
- `cargo fmt --all --check` and `cargo clippy --all-targets --all-features -- -D
  warnings` passed.
- ⚠️ Coverage: OBSERVED-NOT locally (see [M136](../M136/VERIFIED.md)). The floor is CI's
  run on push.
- NOT-RUN here: `scripts/mutants.sh` (`cargo-mutants` is not installed in this container).
