# M123 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.
Every finding and the task closing it: [FINDINGS.md](FINDINGS.md).

⚠️ **Where this ran:** a Linux x86-64 cloud container, 4 cores, the debug profile with
`CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`; the bench and
`scripts/gossip-loss.sh` in release. `provisional` (AGENTS.md). NOT-RUN on macOS and Windows.

1. `a_batched_write_is_flushed_within_the_flush_age` and `a_stop_flushes_and_folds_what_it_holds`
   pass; each **red first** -- the row never reached the store, and a stop left nothing for a
   peer to read.
2. `a_successor_folds_its_predecessors_bundles` passes, **red first** at 0 folded with a fold
   age of an hour; `a_stop_flushes_and_folds_what_it_holds` covers the graceful stop.
3. `a_stop_returns_by_its_deadline_and_still_flushes` passes, **red first** at a 5 s timeout
   on a request held forever. `a_second_signal_exits_at_once`, red with the exit never called;
   `a_halted_tick_starts_no_copy`, red at 10 copies with the halt ignored;
   `a_served_process_runs_its_worker` now also asserts the claim is released, red first.
4. `a_leaving_node_is_dead_everywhere_within_a_few_rounds_and_may_return`,
   `a_member_that_leaves_is_gone_from_its_peers_view_at_once` and
   `a_server_that_leaves_is_dropped_within_a_few_periods` pass, each **red** with nothing
   sent (154 ms and 308 ms against bounds of 150 and 300). `a_stopping_server_is_not_ready`
   passes, red with the flag never set.
5. `the_lane_past_the_ceiling_is_refused_and_the_registry_still_reads`,
   `a_first_window_of_one_probes_an_idle_lane_once`, `a_fold_probes_an_idle_lane_once` (red at
   34 reads for a bound of 6) and `a_folded_lane_is_retired_and_a_later_write_registers_it_again`
   (red with the resume not forgotten) pass. Three tests pinning a fold's reads now pin five
   fewer each, writes unchanged: `a_rejecting_fold_costs_one_write_per_index`,
   `an_uncontended_fold_compaction_and_branch_are_unchanged`, `a_plain_fold_reads_what_it_did_before_m13`.
6. `a_ring_ranks_a_window_and_is_assign_while_the_window_is_the_fleet` (red with the window
   ignored), `a_join_moves_about_one_in_n_keys_mostly_to_the_newcomer` and
   `a_split_places_by_its_peers` (red with `assign` called directly) pass.
7. `an_evicted_tenants_counters_go_with_it` and `a_fold_drops_the_fresh_segments_it_made_stale`
   pass, each **red first**. `eviction_costs_no_request` now counts the process's totals, red
   with a request per evicted tenant. ⚠️ That the eviction pass holds the registry's lock for
   no `is_idle` but the chosen engines' is visible to no test: checked by reading `evict_tick`.
8. `a_dead_member_is_out_of_the_checksum_and_may_be_forgotten`,
   `a_long_dead_member_is_forgotten_everywhere_without_reconciling`,
   `a_strangers_death_is_not_learned` and `a_member_this_node_holds_dead_leaves_the_roster`
   pass; each red against its break -- the second with forgetting off. `scripts/gossip-loss.sh`
   passes at every phase.
9. `the_store_id_is_retried_and_bounded` (red with one attempt) and
   `an_announce_is_bounded_by_its_timeout` (red unbounded, at 5 s) pass. ⚠️ Against a store that
   refuses and one that holds a write, not one that never answers at all; the timeout is the
   same code either way. The disk tiers' overlap is visible to no test: the disk suite passes.
10. `./scripts/query-bench.sh`: every digest equals the baseline's -- `47539a4b450befe3`,
    `64d349ed03f0a2f6`, `65559801f84a1412`, `b06b396d0aa759fb`, `60cf6bfe1f9fc517`,
    `d559aff4b3101b72`, `dd1bf0796e2ba34f`; no time is claimed. `./scripts/gates.sh` passed, run by the pre-commit hook on each task's commit;
    before this one, `cargo test --workspace --all-features --no-fail-fast`: 255 test binaries, none failed.
11. `a_start_scan_reads_every_shard_together` passes, **red first** at 66 rounds for a bound of
    4; `max_bounds_every_worker_and_the_rest_is_shared` fails with each shard handed the whole
    room. `a_core_serves_from_memory_while_its_disk_tier_recovers` (red with a close before
    attach ignored) and `a_start_does_not_wait_for_the_disk_tier` (red with the attach awaited)
    pass. `a_server_splits_only_within_its_own_zone` (red with the zone ignored),
    `a_gossip_zone_is_configured_and_refused_by_name`,
    `a_routed_tenant_is_written_and_read_by_its_owner` and
    `a_tenant_whose_owner_is_down_is_served_where_it_lands` (each red with routing off), and
    `routing_is_off_unless_on_and_refuses_anything_else` pass.
