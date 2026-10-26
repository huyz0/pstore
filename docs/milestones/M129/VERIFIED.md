# M129 verification

Spec written and reviewed in two rounds (round 2: one major on the window's lower bound, fixed in the spec); no M129 code exists yet.

1. NOT-RUN: `applied_counts_are_what_the_fold_decided` is not written.
2. NOT-RUN: `exclusive_conditions_apply_once` is not written.
3. NOT-RUN: `a_request_folded_elsewhere_sums_its_outcomes` and `a_cas_loser_reads_the_winners_outcome` are not written.
4. NOT-RUN: `strong_reads_see_an_applied_write` is not written.
5. NOT-RUN: `req_is_never_sealed`, `outcome_list_is_bounded_and_reaped` and `a_retired_outcome_is_unavailable` are not written.
6. NOT-RUN: `applied_needs_the_rollout_flag` is not written.
7. NOT-RUN: `concurrent_applied_writes_share_a_fold` and `applied_cost_is_one_outcome_put_per_commit` are not written; nothing measured.
8. NOT-RUN: `rejected_rows_are_counted` is not written.
