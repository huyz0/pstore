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
12. **Gates** — `./scripts/gates.sh` on this tree: see the commit. The mutation sweep over the
    diff is NOT-RUN yet: M9h.3's re-run holds the disk. It is added before this lands on
    `main`.
