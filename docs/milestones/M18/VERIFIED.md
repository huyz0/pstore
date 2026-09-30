# M18 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

Engine tests are in `cargo test -p pstore-engine --test reap`, and server tests in
`cargo test -p pstore-server --test scheduled_reap`.

**Observed red.**
- Four of the first five engine tests failed against a stub `reap_due` and `gc_through`. The
  fifth, `gc_by_retention_still_reads_head_once`, guards the refactor of `gc` and passed before
  it.
- The server tests were written with the server code, so each piece of both halves was
  removed by hand instead, and each removal fails a test. The nine removals: the tick doing
  nothing; no backoff recorded; the configuration ignored; the loop not started; `age`
  compared `>`; the records kept after a reap; the records dropped past the horizon; `gc`
  recording its own commit; the horizon ignored.
- Every test added after code review was seen failing first.

1. **Idle costs nothing.**
   - `an_idle_tenant_costs_nothing` covers a tenant only read from, and one never touched.
   - `a_tenant_is_reaped_once_its_commit_is_age_old_and_not_a_second_before` checks that the tick
     an `age` after a reap makes no request.
   - `a_reap_that_finds_nothing_still_forgets_its_records`, and the branch in
     `every_kind_of_commit_is_reaped_on_schedule`, cover a reap with nothing to do.
2. **Due reaps.**
   - Over the API, `a_tenant_is_reaped_once_its_commit_is_age_old_and_not_a_second_before`
     shows the bundles gone.
   - `every_kind_of_commit_is_reaped_on_schedule` (engine) came from code review round 1. It
     covers a compaction's inputs, their delete vector and sidecars, and a dropped index, each
     reaped. Removing the record at each of the three commit sites fails it.
   - `a_reap_takes_what_it_buried_and_forgets_its_records` shows `reaped_before` at the
     horizon, and no graveyard entry at or below it.
3. **Not before `age`.**
   - `a_tenant_is_reaped_once_its_commit_is_age_old_and_not_a_second_before`, a second short of
     `age`: no request, and the bundle is kept.
   - `nothing_is_due_until_a_commit_is_age_old` (engine) failed with `age` compared `>`.
4. **Nothing live is touched.** `the_reap_keeps_what_the_last_age_can_read`: live rows, `as_of`
   at the horizon and later, and `time_travel_horizon` below it.
5. **Bounded by time.** `records_younger_than_age_survive_a_reap` failed with the horizon
   ignored, and with every record dropped.
   - The records' own bound came from code review:
     - `records_stay_bounded_when_nothing_reaps`;
     - `a_tenant_committing_faster_than_a_second_is_still_reaped`;
     - `a_tenant_past_the_record_cap_is_still_reaped`.
   - Round 2 found the first bound starved busy tenants. The last two failed on that bound, and
     fail again with the bucket anchored on `at`, or with the front thinned.
6. **Configured and stopped.**
   - `the_reap_is_configured_by_name`.
   - `the_loop_reaps_on_its_own_and_stops`.
   - `a_served_process_reaps_and_stops_with_its_signal` shows that no reap runs after
     `serve_folding` returns. That the loop was awaited is shown only by that absence, as for
     the fold loop.
7. **Backs off.**
   - `a_failed_reap_backs_off_keeps_its_records_and_recovers`.
   - `the_reap_backoff_doubles_to_its_cap` came from code review round 1. It failed with the
     doubling removed, and with the cap removed.
8. **Cost.** `a_scheduled_reap_costs_what_gc_does`: 1 read, 1 delete batch, 1 CAS.
   `gc_by_retention_still_reads_head_once`.
9. **Gates** — NOT-RUN yet: swept with M17's remainder and M19, then `./scripts/gates.sh` at
   M19's close.

Spec review took two rounds. Code review took three: round 1 blocked on two majors (the records
unbounded; commit kinds untested); round 2 blocked on liveness in round 1's bound; round 3
verified it.
