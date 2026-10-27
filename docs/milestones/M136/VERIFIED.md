# M136 verification

The spec was reviewed in one round, recorded in the [spec](SPEC.md). The code was reviewed
in two rounds: round 1 found two majors, both fixed (below); round 2 passed with minors.

How to run the tests: `cargo test -p pstore-engine --test it filter_rows::`, and the
counter's unit tests with `cargo test -p pstore-engine --lib filter_rows`.

Red runs: the builder landed first as a stub that stored the bound and did nothing else,
since every engine test is one binary. Against the stub, criterion 1's test passed (a
guard), and criteria 2, 3, 4 and 6 failed: each refused operation applied.

The mutations were applied one at a time by a script and then reverted, running the
engine's `filter_rows::`, `patch::` and `applied::` tests and the unit tests. All eleven
were killed. No `cargo mutants` run is claimed for them.

1. `a_filter_admitting_the_bound_applies`: green after. Killed by "the scan bound
   compares `>=`" and "the fold-order bound compares `>=`".
2. `a_filter_admitting_more_than_the_bound_is_refused_whole` (delete and patch): green
   after. Killed by "the scan count is skipped" and "a scan refusal counts `skipped`".
3. `fold_order_counts`: green after. Killed by "the fold-order count is skipped" and "a
   fold-order refusal counts `skipped`".
4. `the_scan_counts`, and `each_operation_reads_its_own_scan_count` (added in code review:
   a second by-filter operation must read its own count): green after. Killed by "the scan
   count is skipped", "a row a per-id operation names is not counted", and, for the second
   test, "the operation index never advances".
5. `retention_stops_at_the_bound`, `the_bound_itself_is_within` and
   `an_unread_filter_admits_nothing` (unit tests): green after. Killed by "retention
   ignores the count". The clause "a row a per-id operation names is retained" lives in the
   fold's closure, not the counter: "named rows dropped" is killed by the existing
   `applied::` and `patch::` tests, among them
   `a_patch_after_a_delete_supersedes_the_right_row_across_blocks`.
6. `the_fold_holds_at_most_one_row_past_the_bound`: green after. It asserts the fold kept
   exactly the bound (one filter, so the count is deterministic); code review found the
   first version's `<=` let a hook reading 0 pass. Killed by "retention ignores the
   count" and "the kept count is not summed".
7. `cargo test -p pstore-engine` passed after (528 integration tests, 34 unit tests), and
   `cargo test -p pstore-server --test it` (417).

Gates, in a cloud container:
- `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings`
  passed.
- ⚠️ Coverage: OBSERVED-NOT locally. `scripts/coverage.sh --all-features
  --ignore-run-fail` ran every test to completion but printed "190 functions have
  mismatched data" and read `pstore-engine/src/lib.rs` at 59%, so llvm-cov dropped counts
  in this container. `filter_rows.rs` read 100%. The floor for this change is CI's run on
  push.
- NOT-RUN here: `scripts/mutants.sh` (`cargo-mutants` is not installed in this container).
