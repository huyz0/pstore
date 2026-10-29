# M149 verification

Implemented in `crates/pstore-engine/src/head.rs` (the `read_only` section after `times`,
its forcing of the times' count, and the existence filter in `encode`),
`crates/pstore-engine/src/lib.rs` (`EngineError::IndexReadOnly`, `set_read_only`, the
read-only names beside the replica names, the write door's refusal, the fold's
set-aside into the quarantine, the drop's refusal, and `IndexStats::read_only`) and
`crates/pstore-server/src/lib.rs` (the `PATCH /v1/indexes/{index}` route, the `409
index_read_only` and `409 index_not_folded` answers, and the summary's `read_only`). All
runs are in a 4-core cloud container, and every number is `provisional`.

How to run: `cargo test -p pstore-engine --lib head::`,
`cargo test -p pstore-engine --test it read_only` and
`cargo test -p pstore-server --test it read_only::`.

Red run (.harness/m149-red-head.log): `read_only_round_trips_beside_times` was written
before the encoding and failed against it (the flags did not survive a round trip). The
engine and server tests were written after the code they call existed, so their red is
the mutations below.

Mutations, applied one at a time by script and reverted, each running the head unit
tests, the engine `read_only` tests (the replica's included) and the server
`read_only::` tests (.harness/m149-mutations.log). The script reports a compile error as
invalid, not as a kill, and none was invalid. All 21 were killed:
- the door not knowing the flagged names, not refusing after its read, or not
  remembering the names a HEAD read carries;
- the drop not refused;
- the fold not setting rows aside, not reporting them rejected, not quarantining them,
  not counting them, or exiting early past them;
- `set_read_only` without its replica refusal or its existence rule, committing the
  value the index already has, inserting when clearing, or not remembering its commit;
- `encode` without its filter, or the flags not forcing the sections before them;
- the stats' flag without the replica or without the owner's flag;
- the summary's flag dropped, `index_not_folded` not answered, and `index_read_only`
  answered as 400.

No `cargo mutants` run is claimed.

1. PASS: `read_only_survives_restart_and_refuses_writes`. The summary reports the flag
   from the `Api` that set it and from a second `Api` over the same store. A third `Api`
   sees it cleared after the second clears it, and writes.
2. PASS: `read_only_survives_restart_and_refuses_writes`,
   `a_read_only_index_refuses_every_write_kind`.
   - Through the API, from both processes: an upsert, a patch, a delete, a conditional
     upsert, a delete by filter and a patch by filter are each `409 index_read_only`,
     and a drop is too. The store's PUT count is unchanged across them and a fold.
   - On the engine, every write method is `IndexReadOnly`, from the process that set it
     and from one whose first write reads HEAD. A fold after them commits nothing.
3. PASS: `a_stale_door_is_set_aside`.
   - Rows from a process that read HEAD before the flag, accepted before and after it,
     are set aside by the next fold: an upsert, an applied upsert, a delete and a patch.
   - The served rows are unchanged, and `rejected_rows` and `quarantined_rows` are 4.
   - The quarantine holds all four ids, and the applied upsert's verdict is 0 upserted,
     1 rejected.
4. PASS: `a_fold_losing_to_the_flag_sets_aside`. A drop's fold carrying another process's
   row loses its CAS to the flag. On its retry the row is set aside and not served, and
   the drop still lands.
5. PASS: `a_stale_door_is_set_aside`. After clearing, a write folds and is served, and
   the quarantine still holds exactly the four set-aside rows.
6. PASS: `read_only_lifecycle`, `a_replica_is_read_only_by_its_replication`,
   `setting_the_value_it_has_commits_nothing`, `patch_refusals`.
   - A branch of a read-only index is writable. A drop is refused while flagged and
     succeeds once cleared, and the name created again is writable and not flagged.
   - A replica reports `read_only`, and setting or clearing the flag on it is
     `replica_read_only`. A read-only source still syncs to its replica.
   - `PATCH` is 404 on an absent name, and `409 index_not_folded` on a name held only
     unfolded. The engine commits nothing for either.
   - A missing, mistyped or extra field is 400.
7. PASS: `read_only_round_trips_beside_times`.
   - The flags round-trip beside each combination of absent and present `keys_since` and
     `times`.
   - A HEAD whose flags name only absent indexes encodes byte for byte as one without
     flags.
   - A trailing byte is refused, and no truncation decodes flags.
8. PASS: `read_only_survives_restart_and_refuses_writes`,
   `setting_the_value_it_has_commits_nothing`.
   - Setting the flag costs one read and one write, by the response's `cost` and by the
     store's counters.
   - Setting the value it has costs no write and answers the same epoch.
   - No existing request-count test changed: the workspace suite below runs them
     unmodified.
9. PASS, with the IPv6 exception M127 records.
   - `cargo clippy --all-targets --all-features -- -D warnings` passed
     (.harness/m149-clippy.log).
   - The workspace suite passed 2,364 tests with none failing
     (.harness/m149-quality-suite.log). It ran with all features and both `[::1]` tests
     skipped: the container has no IPv6, and those tests run in CI.
   - `./scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` passed at
     97.20% lines and 95.78% regions (.harness/m149-coverage.log).
   - `scripts/recall.sh`, `scripts/ndcg.sh` and `scripts/depth.sh` passed
     (.harness/m149-quality.log).

Rebased after M173, whose recorded types precede M148's `times`. The flags force the
times' count and through it the types' count (0 when none). `read_only_round_trips_beside_times`
gained a round trip beside recorded types, live and dropped, with and without times, and
fails with the flags forcing the times but not the types (.harness/m149-m173-mutations.log).
The engine and server suites passed after the rebase (.harness/m149-m173-suite.log).

Code review: see the commit message.

CI's mutation shard missed `-` → `+` and `-` → `/` in `set_read_only`'s retry guard
(`attempt < MAX_COMMIT_ATTEMPTS - 1`): no test made every attempt contend, the one case
where the last error tells `Contended` from `Lost`. M86's
`a_flag_that_keeps_contending_reports_contention_after_every_attempt` now does, and fails
under both (.harness/m149-ci-mutation-red.log).
