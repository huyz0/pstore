# M278 verification

The spec was not sent to a separate reviewer before code; the PR's review covers it.

1. `cargo test -p pstore-testkit --test it budget`: `an_exact_budget_fails_above_and_below`,
   `a_max_budget_fails_only_above`, `a_per_unit_budget_scales_with_its_units` and
   `spending_less_than_an_exact_budget_fails` pass (11 tests in the filter, all green).
2. Same run: `a_misspelt_bound_fails_rather_than_vanishing`,
   `a_missing_count_fails_rather_than_vanishing` and `an_unknown_name_fails` pass, each a
   `should_panic` naming its message, with `the_table_parses_to_its_rows_and_nothing_else` and
   `the_checked_in_table_is_read`.
3. Converted tests, green against the table:
   `cargo test -p pstore-server --test it -- cost:: depth:: puts_per_write` (12 passed, among
   them `a_batched_write_costs_nothing`, `a_durable_write_costs_its_lane_registration_once_then_one_put`,
   `a_memtable_query_reads_head_and_nothing_else`, `a_query_costs_four_round_trips_however_many_segments_it_has`)
   and `cargo test -p pstore-engine --test it -- end_to_end cold_depth` (3 passed, among them
   `the_whole_flow_stays_inside_its_request_budget` and `a_cold_query_from_head_costs_three_round_trips`).
   Red: with `durable_write_puts` raised from 1 to 2 in the table, three server tests failed,
   each with "spent 1 requests, under its exact budget of 2" (or 100 of 200, 64 of 128):
   `a_durable_write_costs_its_lane_registration_once_then_one_put`,
   `a_sparse_durable_write_costs_one_put_per_tenant_and_a_fold_two`,
   `concurrent_durable_writes_to_many_tenants_cost_one_put_each`. Reverted, green again.
4. `./scripts/check-budgets.py`: "ok 15 request budgets, each spent by a test". Its
   `--self-test` passes, and refuses an unspent row and an unknown name. Before
   `end_to_end.rs` was written with one literal name a call it refused four rows that a
   variable spent, which is why each call now names its row literally.
