# M212 verification

How to run the tests: `cargo test -p pstore-server --lib ingress` and
`cargo test -p pstore-server --test it -- query_reservation::`.

Red runs: with the limits and the pool in place but the handler reserving nothing, all four
`query_reservation::` tests failed (every boundary query and every contended query was
answered 200, and nothing was held mid-run). The two new `ingress` unit tests did not compile
on the parent (no `query_rows`), as the spec says.

Mutations were applied one at a time by a script and reverted, against the `ingress` unit
tests and `query_reservation::`. 9 of 9 were killed. No `cargo mutants` run is claimed.
- legs not counted; the window not counted; a faceted query's facets not counted; a
  multi-query's queries after the first not counted; `>=` at the tenant bound; the single
  query's guard dropped before the run; the multi-query's guard dropped before the run
  (killed once the held test also parks a multi-query); the server pool not checked; 429
  rather than 413 for a plan that can never fit.

1. `every_leg_and_sub_query_counts_up_to_the_tenants_pool`: green after, at R = 60, the
   hybrid query at `top_k` 20 and 21, the multi-query at 60 and 61, with no blob request on
   the 413. Killed "legs not counted", "later sub-queries not counted", ">= at the bound".
2. `windows_offsets_aggregations_and_facets_each_count`: green after for a boost window, a
   ranked `offset`, an order's `offset`, an aggregation's `top_k` and a facet's `top_k`.
   Killed "window not counted" and "facets not counted".
3. `a_short_pool_is_overloaded_and_another_tenant_is_not` and the unit test
   `a_query_reservation_is_bounded_per_tenant_and_in_all_and_returned_on_drop`: green after;
   429 with `Retry-After: 1` and no blob request. Killed "server pool not checked".
4. `a_reservation_is_held_while_running_and_returned_on_every_path`: green after; 21 rows
   held while a hybrid query at `top_k` 7 is parked, 13 for a parked multi-query, 0 after a
   success, a 404, a 413 and a 429. Killed both guard mutations and "429 for never-fit".
5. `query_row_limits_follow_m128s_rules`: green after.
6. `cargo test -p pstore-server --no-fail-fast` passed (the `it` suite: 662 passed, 1
   ignored), skipping the two IPv6 peer tests this container cannot run.

7. `an_external_query_reserves_its_window`: added after code review found the
   `/external/query` route reserved nothing. Seen red first (window R + 1 was answered by
   the engine, 403 `external_sources_off`, not 413), green after.

Gates, in a cloud container: `cargo fmt --all --check` and `cargo clippy -p pstore-server
--all-targets -- -D warnings` passed. Coverage and `scripts/mutants.sh`: NOT-RUN here; CI
runs both on the pull request.
