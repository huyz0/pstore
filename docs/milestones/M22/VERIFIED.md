# M22 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0). Every
count is a request count, exact on any machine; no number here is a latency.

Commands: `cargo test -p pstore-jobs`, `cargo test -p pstore-engine --test replicate`,
`cargo test -p pstore-server --test replication`.

**Observed red.**
- M22.1: 17 of 19 queue tests failed on stubs. The interleaving search failed on the first
  implementation, which is how the touch and version rules were found.
- M22.2: every `replicate.rs` test failed on stubs.
- M22.3: tests were written first but **not run against stubs**; their red is by mutation:
  27 hand mutations, each failing a test.
- Tests that pass on a stub guard a mutation, named below.

1. **Follows.** `follows_a_same_tenant_source`, `follows_a_cross_tenant_source`,
   `follows_a_remote_source` (writes, deletes, a second delete in one segment, compaction,
   drop-and-recreate, a branch with deletes, an equal-count re-branch, a re-branch with none;
   rows, dense, filtered, sparse and text answers, and the schema). Server:
   `a_remote_source_is_followed`. Mutations killed: refs reversed, schema not copied, a vector
   kept by existence, a vector of a dropped segment kept.
2. **Incremental.** `copies_only_what_changed`, `an_idle_sync_reads_once_per_source`,
   `two_replications_of_one_source_read_it_once`. Killed: `known` ignored. ⚠️ Amended at
   M22.2: sidecars are all optional, so "expected" is "existing".
3. **GC-safe.** `compaction_and_gc_on_both_sides_keep_the_replica_whole`,
   `a_source_segment_reaped_mid_copy_is_remapped`,
   `a_sidecar_reaped_mid_copy_is_remapped_not_taken_as_absent`,
   `a_branch_from_a_replica_survives_the_replica_moving_on`, `a_lost_commit_reuses_its_copies`,
   `a_rivals_duplicate_copies_are_buried`, `a_failed_replications_copies_are_buried`,
   `an_abandoned_sync_buries_what_it_wrote`,
   `a_sync_that_cannot_reread_its_head_buries_what_it_wrote`.
   - Every one checks that nothing under the dest index is left unnamed after `gc(0)`.
   - Killed: no burial of replaced segments or unused copies, copies remade per attempt, a
     sidecar 404 taken without a fresh HEAD, a remap on a stale source.
4. **No regress.** `an_older_source_never_commits`, `a_regressed_source_fails_without_copying`.
   Killed: the check dropped, and the check after copying. `<=` as `<` is **equivalent**: an
   equal source epoch is an equal fingerprint, already current.
5. **Read-only.** `a_replica_refuses_everything_that_would_write_it` covers eleven write and
   history paths. Also: `a_refusal_from_a_stale_cache_is_re_read_before_it_stands`,
   `a_row_past_a_stale_door_is_counted_and_never_served` (with a scan),
   `cancel_leaves_a_normal_index_whose_history_starts_there`, `create_refuses_by_name`,
   `a_compaction_or_branch_that_races_a_new_replica_never_lands_in_it`; server:
   `every_refusal_is_named`. Each refusal removed by hand fails one.
6. **Pause fences.** `a_pause_fences_a_racing_sync`, `a_resume_over_an_idle_source_commits_nothing`,
   `one_failing_replication_does_not_stop_another`, `a_cached_plan_never_commits_another_sources_data`;
   server: `a_new_replication_on_a_held_tenant_syncs_within_a_renewal` (16 shards, so no scan
   stands in for the renewal). Killed: the state, run and source checks; a resume keeping its
   run; a failure aborting the sync; the renew-time `gen` check.
7. **Queue.** `claim_takes_unclaimed_expired_and_own_and_skips_live_foreign`,
   `claim_honours_room_and_never_counts_its_own`, `claim_leaves_its_own_live_claims_alone`,
   `renew_extends_its_own_and_drops_what_it_lost`, `config_is_created_once_and_its_count_wins`,
   `two_claimants_take_each_entry_once`, and 17 more in `queue.rs`. 16 hand mutations each fail
   one.
8. **Reconcile.** `two_controllers_and_a_worker_from_nothing`,
   `two_controllers_and_a_worker_from_work`, `a_resume_racing_a_pause_and_a_new_generation`,
   `renew_and_claim_never_bring_back_a_removed_entry`,
   `renew_and_claim_never_lose_a_new_generation`, `the_enumeration_reaches_a_losing_write`, under
   both tag styles.
   - ⚠️ "Exhaustive" is **preemption-bounded at 3**, exhaustive within the bound. The unbounded
     search ran over 30 minutes and was stopped. The bug it found needed 2 preemptions.
   - Killed: touch ignored, no version bump, truth read before shard, a plain `put`.
9. **Workers.** `three_workers_converge_and_share_the_work`, `a_dead_workers_tenants_move`
   (within `ttl + S·scan`), `a_creator_with_a_worker_claims_its_job_at_once`,
   `a_full_worker_does_not_claim_at_creation`, `a_worker_removes_an_entry_with_nothing_running`,
   `a_paused_job_costs_nothing_once_its_claim_is_dropped`, `a_served_process_runs_its_worker`.
10. **API.** `a_job_is_created_followed_paused_resumed_listed_and_cancelled`,
    `every_refusal_is_named`, `the_list_costs_one_read`, `a_status_call_repairs_a_lost_entry`,
    `every_control_call_leaves_the_register_right_by_itself`,
    `a_worker_without_the_named_store_says_why`, `the_last_cancel_takes_the_status_notes_with_it`,
    `a_remote_sources_reads_are_billed_to_the_replicas_tenant`,
    `the_worker_and_its_sources_are_configured_from_the_environment`.
11. **Cost.** `a_worker_at_rest_costs_what_the_spec_says`: over 600 s at rest, exactly 60 shard
    reads, 15 renewal reads and CASes, 10 source reads for two replicas of one source, and 0
    tenant writes. It found `claim` rewriting a worker's own live claims on every scan, which
    was fixed and given its own test.
12. **Gates** — NOT-RUN yet: the sweep over the source diff and `./scripts/gates.sh` are in
    progress.

Spec review took three rounds: round 1 blocked (2 blockers, 10 majors), round 2 blocked (2
liveness holes), and round 3 approved. Code review: M22.1 passed in one round. M22.2 blocked in
round 1 (a cached plan could commit another source's data) and passed in round 2. M22.3 is in
review.
