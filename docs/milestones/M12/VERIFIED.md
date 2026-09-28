# M12 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

Every server test below is in `cargo test -p pstore-server --test aggregate`. **Observed
red:** all eight first-written tests failed before the implementation. `aggregate_by` was
ignored and each query 400'd for having no vector, and the refusal test failed because an
aggregation beside a vector was accepted.

1. **Count and sum** — `counts_and_sums_equal_brute_force`.
   - The data: 2,000 rows in 4 folds, a folded delete, and unfolded writes including an
     upsert and a delete of folded ids.
   - `Count id`, `Count color`, `Sum` of ints, and `Sum` of a mixed column (within `1e-9`)
     all equal the model, with and without a filter, and `as_of` a past epoch.
   - `sums_are_typed_as_stated` (`cargo test -p pstore-query --lib aggregate`) pins the
     typing: exact ints, a float sum, i64 overflow to f64, ±∞ to null, and an empty sum of 0.
2. **Groups**
   - `groups_equal_brute_force`: one attribute, and two lexicographically.
   - `the_smallest_keys_are_exact_across_segments`: `top_k = 3`, where a global top-3 key is
     one segment's own 3rd smallest.
   - `numbers_and_arrays_group_by_value`: `1` and `1.0` as one group, absent last,
     `[1]` = `[1.0]` before `[1, 2]`.
   - `it_holds_at_most_top_k_groups_and_keeps_the_smallest_exactly` (1,000 keys, never more
     than 3 held) and `a_merge_is_exact_when_a_global_key_is_a_segments_last_kept`.
   - `keys_order_and_unify_as_stated`, which covers the order and `-0.0`, 2^60 and 2^63.
   - `unfolded_hits_counts_only_rows_aggregated`, added at code review round 2.
3. **Fast path** — `an_unfiltered_count_is_heads_arithmetic`:
   - 1 read, equal to the full path, which costs more;
   - the full path when a write is unfolded;
   - `as_of`, and 404 for a missing index.

   Three more tests came from code review:
   - `an_unfolded_delete_alone_takes_the_full_path` (M2). With the shadow clause removed by
     hand, it failed.
   - `a_past_count_survives_a_compaction` and `a_past_count_survives_a_replaced_delete_vector`
     (`cargo test -p pstore-engine --test aggregate`, B1). With the live-only guard removed,
     both failed: 0 against 2,000, and 2,000 against 1,999.
4. **Depth** — `a_filtered_aggregation_is_three_rounds_deep`: at most 3 under
   `DepthCounting`.
5. **Consistency** — `every_level_applies`, on both shapes:
   - `strong` and `session` are refused;
   - `bounded` reports `staleness_ms`, a hit reads no HEAD, and a fast-path hit reads nothing.

   Two tests came from the sweep:
   - `a_bounded_hit_on_a_reaped_segment_falls_back`: the filtered shape answers 404, not
     500. An unfiltered count answers from the cached HEAD, touches no reaped object, and
     is `bounded` doing its job.
   - `a_refused_strong_aggregation_is_not_run_twice`: pinned at 5 reads.
6. **Refusals** — `what_an_aggregation_cannot_mean_is_refused`. Each is `400`, and its message
   names the rule. `an_aggregations_top_k_is_bounded_as_an_orders_is` refuses 10,001 and
   accepts 10,000.
7. **Gates**
   - `./scripts/mutants.sh --check . --in-diff <the M12 source diff>` after code review:
     **111 tested in 82m, 87 caught, 20 unviable, 4 missed.** Three were killed and one is
     equivalent:
     - `hit` became `true`, then `false`, in `aggregate_as`'s fallback. Both are now killed by
       the two sweep tests in criterion 5, each seen failing by hand.
     - The datetime arm of `Key::cmp` was deleted. It is now killed by
       `keys_order_and_unify_as_stated`, seen failing by hand.
     - ⚠️ `<` became `<=` in `Aggregator::admit`. This one is equivalent: a key equal to the
       largest kept one has already returned through `contains_key`, which uses the same
       order.
   - The first sweep, on the pre-review tree, was stopped when code review changed the
     engine. Its partial results are not claimed.
   - Spec review: two rounds.
     - Round 1 had four majors: array order, the fast path's level costs and its
       same-view condition, and the memory bound.
     - Round 2 had one major, a criterion claiming a filtered bounded hit reads nothing, which
       was fixed.
   - Code review: two rounds.
     - Round 1 blocked on the fast path under `as_of`, and was fixed.
     - Round 2 passed, with two minors:
       - `unfolded_hits` had no test, which was added;
       - the M13 spec rode in M12's work-in-progress commit. Noted: that commit is pushed.
   - `./scripts/gates.sh` on this tree: all seventeen PASS.
