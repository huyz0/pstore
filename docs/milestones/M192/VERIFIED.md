# M192 verification

Implemented in `crates/pstore-engine/src/replica.rs` (the sync gate, `SyncState::sources`),
`crates/pstore-engine/src/lib.rs` (the gate's field) and `crates/pstore-server/src/replication.rs`
(the bound, `Held::judge`, the notes, the status fields, `sync_now`), with the route, metrics
and `PSTORE_REPLICATION_MAX_LAG_S` in `crates/pstore-server/src/lib.rs`. All runs are in a
4-core cloud container under MemoryStore and paused tokio time; every number is `provisional`.

How to run: `cargo test -p pstore-engine --test it replicate::a_second_sync_waits_for_the_first`,
`cargo test -p pstore-server --test it -- replication_lag auth_sources::a_sync_now` and
`cargo test -p pstore-server --lib judge_`.

Red runs: the engine test failed against the engine without the gate ("the second sync ran
while the first was mid-sync", .harness/m192-red-engine.log). The server tests ran against a
server whose policy had `max_lag` and nothing else (.harness/m192-red-server.log): ten failed,
and `without_a_bound_it_waits_for_the_fold` passed, as the control it is. The status test then
failed once more against the first implementation (`fresh_ms` null for `ttl/3` after start),
which is why a replication's first fresh time is written at once.

Suite, under `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95`: 2,652 passed,
0 failed, the two IPv6 tests skipped (no IPv6 in the container). Lines 97.29%, regions 95.93%
(.harness/m192-coverage.log). After the review fixes below, the replication, replication_lag
and auth_sources tests ran again: 56 passed (.harness/m192-final-server.log); fmt and clippy pass.

Mutations, one at a time by script and reverted (.harness/m192-mutations.log). Killed: fold
cadence `L` for `L/2`; source authority unchecked; paused not refused; the tenant's due time
ignoring the fold time; the first fresh time not written at once; and, by the unit test added
after they survived the integration tests, `>` as `>=` at the bound, counting per sync instead
of per change, and taking a fresh time from a fold whose sync did not find the replica current.
A fourth survivor, "a remote source folded", was a redundant filter: `judge` already skips a
remote replication, so the filter was removed and the unit test pins the skip. NOT-RUN: the
sync-now `fresh_ms` guard (`dest` neither committed nor current after a sync that did not fail)
needs a pause landing between the status read and the sync, and no hook places one there.
No `cargo mutants` run is claimed.

1. PASS: `a_write_reaches_the_replica_within_the_bound`: an unfolded source write reaches the
   replica within 10 s of paused time, folded only by another process's worker;
   `without_a_bound_it_waits_for_the_fold`: without the bound it does not within 100 s.
2. PASS: `an_idle_source_stays_bounded`: after 300 idle seconds a write still arrives within
   `L`, and the source was folded 55-70 times in ~310 s (every `L/2`).
3. PASS: `status_reports_the_lag`: `max_lag_ms` 10,000, `fresh_ms` set, `lag_ms` within `L`
   and advancing with the clock, `lag_exceeded` false. `a_remote_source_is_not_bounded`:
   `max_lag_ms` and `fresh_ms` null, the remote write not folded, no source fold counted.
4. PASS: `a_failing_source_fold_is_exceeded_and_recovers`: unreadable source bundles make
   `lag_exceeded` true with `last_error` naming the source fold, `lag_exceeded_total` 1,
   `lagging` 1 and failed folds counted; readable again, false within one fold, `lagging` 0,
   the counter still 1, and the row arrives. Unit: `judge_marks_past_the_bound_once_and_only_from_a_current_fold`.
5. PASS: `sync_now_folds_the_source_and_syncs`, `sync_now_refuses_by_name` (404, 409, and 503
   naming the source fold), `sync_now_over_a_remote_source_syncs_without_folding`,
   `sync_now_finishes_a_copy`, and `a_sync_now_needs_the_sources_authority` (403 with no source
   request, then 200 with the grant).
6. PASS: `a_second_sync_waits_for_the_first`: the second sync waits while the first is held,
   both end with the replica equal to its source, nothing leaked.
7. PASS: `replication_max_lag_is_validated`: unset is none, `2` is accepted, `1`, `0` and `x`
   are refused by name.
8. PASS: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
   `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` over its floors; the
   other fast gates ran individually (`check-verified`, `check-links`, `check-milestone-ids`,
   `build-index --check`, `check-slos`, `check-poison`, `check-portable`). `cargo deny` is
   NOT-RUN here; CI runs the whole of `scripts/gates.sh`.
