# M9i — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

## M9i.1 — a scheduled fold

All in `cargo test -p pstore-server --test scheduled_fold`. Every test in the file was
**observed red** at first, because the API it names did not exist. The behaviours in 2, 6
and 7 were also **seen red** with the one fix each rests on removed.

1. **Another process sees the write** — `another_process_sees_a_durable_write_with_no_admin_call`:
   invisible to B, then found after one `A.fold_due`, and a second tick folds nothing.
2. **Idle costs nothing** — `a_tenant_with_nothing_to_fold_costs_no_request` covers (a)
   folded here, (b) batched only, and (c) folded by another process, with zero requests
   after at most one `nothing`. `a_flush_that_lost_to_a_fold_leaves_nothing_due` covers
   (d): the bundle's PUT is held after it lands, while an admin fold commits it. Red, (c)
   with the `nothing` prunes removed, and (d) with `unfolded()` counting batches below
   `pruned`.
3. **Size triggers** — `the_byte_threshold_picks_the_larger_tenant`, and
   `bytes_are_summed_across_batches_and_met_exactly`: two batches, each under the threshold,
   fold at exactly their sum and not one byte short. That test was added at code review,
   because every other tenant held one batch.
4. **Age triggers** — `the_age_threshold_is_exact`, under the paused clock: not folded at
   59 s, folded at 60 s. `age_is_measured_from_the_oldest_batch` (code review) was seen red
   with the age taken from the newest batch.
5. **The loop runs, and stops** — `the_loop_folds_on_its_own_and_stops`, under the paused
   clock, and `a_served_process_folds_and_stops_with_its_signal`, on a real socket and clock.
   Nothing is billed to a tenant for 200 ms after `serve_folding` returns, with a write left
   due.
6. **Failure backs off** — `a_failing_fold_backs_off_and_recovers`: at most 8 attempts in 64
   ticks, and folded once the fault clears. With the backoff check disabled it made 64.
   `the_backoff_doubles_to_its_cap_and_resets` (code review) pins the schedule exactly,
   0, 1, 3, 7, then every 8, and checks that a success resets it. It was seen red with the
   reset removed.
7. **A fold does not block requests** — `a_fold_in_flight_does_not_block_other_tenants`:
   one tenant's HEAD read is held while another tenant's write completes. With the engines
   lock held across the tick, the write waited out the 5 s timeout.
8. **Two folders, one tenant** — `two_processes_fold_one_tenant_without_failing`.
9. **Metrics** — `folds_are_counted_by_outcome` (`folded` at 1, no tenant id) and
   `a_failing_fold_backs_off_and_recovers` (`failed` at 1 after its first tick).
10. **Config** — `the_fold_policy_is_configured_or_refused`: the defaults, `off`, set values,
    and each of `0`, `-1`, `x` and `PSTORE_FOLD=on` refused by name. A bad value beside `off`
    is refused too (code review).
11. **The duty tells the truth** — `the_fold_duty_tells_the_truth` checks `/v1/admin/duties`
    and `docs/deploy.md`: no claim that bundles are read on every query, a scheduled fold
    named, and the four variables documented.
    `the_duties_endpoint_reports_every_unscheduled_duty` (`cargo test -p pstore-server --test
    deploy`) still holds.
12. **Gates** — `./scripts/gates.sh` on this tree: see the commit. The mutation sweep:
    `./scripts/mutants.sh --in-diff`, run one source file at a time over M9h.3, M9i.1 and
    M9i.2 together (`git diff 25e43b5..HEAD`), each shard committed under
    [`sweep/`](sweep/) as it finished. Container restarts had killed three
    whole-diff runs.
    - **Totals: 403 tested, 366 caught, 34 unviable, 3 missed.**
    - `datetime.rs`: 246 of 246 caught.
    - All three misses were in the server's `lib.rs`: the scheduled fold's `nothing` count, an
      explicit `"eventual"`, and an overflow fallback that could itself overflow.
    - Each got a test, and the fallback became a fixed far future.
    - The re-sweep `--check 'fold_tick|in level' --file crates/pstore-server/src/lib.rs` then
      missed 5 of 36, all in that new fallback: the "absurd" test's period still fit in an
      `Instant`. It now uses u64::MAX seconds and advances five years, and all five mutants
      were each seen caught by hand.
    Code review: two rounds (block on two test gaps, then pass).

## M9i.2 — `consistency`

All in `cargo test -p pstore-server --test consistency`. Every test in the file was
**observed red** at first: `consistency` was ignored, and the API it names did not exist.

1. **Strong refuses what it cannot see** — `strong_refuses_what_it_cannot_see`:
   - lane A has a watermark of 1 and one unfolded bundle;
   - `strong` through B is `503 not_folded`, retryable, with `Retry-After`;
   - `eventual` does not see the write;
   - `B.fold_due` under a policy that folds only requested tenants folds it;
   - `strong` then serves both rows.
2. **Strong serves what it can** — `strong_serves_what_it_can`, with a lane registered that
   holds no watermark.
3. **Own lane** — `the_own_lane_is_probed_past_what_this_process_holds`: served, not refused,
   at exactly 3 more reads than `eventual`, one registry GET and two probes. Seen red with the
   own lane probed at its watermark rather than `next`.
4. **A dead writer's lane** — `a_dead_writers_lane_is_not_trusted_after_a_restart`. The folded
   bundle 0 is reaped first, so only a probe at the watermark, not at the restarted engine's
   `next` of 0, finds the unfolded bundle (code review). Seen red with `max(watermark, next)`
   replaced by `next`, which cargo-mutants cannot generate.
5. **Depth** — `strong_costs_no_extra_round_trip`: three lanes, folded segments, a filtered
   relevance query and a `rank_by` order. `strong`'s depth is at most `eventual`'s, which
   measured 4 for the filtered query; the probes overlap it and measured 3. With the probes
   awaited one lane at a time after the query, it went red. The spec's earlier "never over
   3" was corrected: this `eventual` query is itself 4 rounds.
6. **Rank orders too** — `strong_refuses_for_a_rank_order_too`.
7. **A probe error is an error** — `a_probe_that_fails_is_an_error`: `503
   storage_unavailable`.
8. **One fold for many refusals** — `many_refusals_ask_for_one_fold`: eight refusals, one
   fold, then nothing due.
9. **Refusals** — `what_consistency_cannot_mean_is_refused`: the five values, `strong` with
   `as_of`, a multi-query with a bad sub-query, and `null` served as `eventual`.
10. **Reported** — `each_sub_query_reports_its_level`, and the `meta.consistency` assertions
    in 1 and 2.
11. **Docs** — `the_research_is_corrected_where_it_promised_otherwise`.
12. **Gates** — code review: one round, pass, with four minors:
    - the `max` test in 4, taken;
    - the engine also refusing strong with as_of, taken;
    - no HEAD clone for an `eventual` order, taken;
    - a failed requested fold keeping its mark, untested and left as a minor.
    The mutation sweep is M9i.1's criterion 12 above, one sweep over all three changes.
    `./scripts/gates.sh` on this tree: see the commit.

⚠️ **Found at spec review, outside this task, and carried to BACKLOG:**
- ⚠️ **Corrected by [M9j](../M9j/SPEC.md):** a restarted engine now resumes its lane at the tail.
- A server restarted on its stable lane starts at sequence 0 and can overwrite its own unfolded
  bundles.
- `meta.epoch` reports the process's last commit, not the HEAD it served.
