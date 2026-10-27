# M137 verification

The spec was reviewed in one round, and the code in one (pass with minors), both recorded
in the [spec](SPEC.md).

How to run the tests:
- engine: `cargo test -p pstore-engine --test it filter_rows::`, and the outcome object's
  unit tests with `cargo test -p pstore-engine --lib applied`;
- server: `cargo test -p pstore-server --test it allow_partial::`.

Red runs: the engine's `delete_by_filter_partial` and `patch_by_filter_partial` landed
first as stubs calling the plain operations, with `Outcome::rows_remaining` added and
never set; the server's two request fields and setters landed first unread. Against the
stubs, criteria 1, 2, 4, 5 and 7 failed; 3 passed (a guard). Criterion 6's test was
written after the section; its red is the two section mutations below.

The mutations were applied one at a time by a script and then reverted, running the
engine's `filter_rows::`, `applied::` and `idempotency::` tests, its `applied` unit tests,
and the server's `allow_partial::` and `applied_writes::` tests. All fourteen were killed.
No `cargo mutants` run is claimed for them.

1. `a_partial_delete_resumes_until_nothing_remains`: green after. Killed by "a partial
   operation is refused like a plain one" and "`rows_remaining` is never set".
2. `a_partial_patch_counts_only_rows_it_would_change`: green after. Killed by "the scan
   counts admissions for a partial patch" and "a partial operation is refused like a plain
   one".
3. `a_partial_operation_at_the_bound_leaves_nothing`: green after. Killed by "rows remain
   when the bound is reached rather than passed".
4. `a_partial_operation_counts_in_fold_order`: green after. Killed by "the fold-order count
   does not set `rows_remaining` for a partial operation".
5. `a_partial_operation_takes_only_rows_it_would_change`: green after. Killed by "fold
   order takes every admitted row" and "the scan counts admissions for a partial patch".
6. `rows_remaining_round_trips_after_the_m132_bytes` (unit): green after. Killed by "the
   remaining section is not written" and "it is ignored on decode". The M129, M130 and
   M132 decoding tests passed unchanged.
7. `a_partial_delete_says_what_remains`, `a_partial_patch_says_what_remains` and
   `allow_partial_refusals`: green after. Killed by "the server drops the delete flag",
   "... the patch flag", "accepts it without `applied`", "accepts it below level 4",
   "accepts it without its operation" and "never says `rows_remaining`". The level
   parsing in `main.rs` is not tested: binary entry points are not.
8. `cargo test -p pstore-engine` passed after (533 integration tests, 35 unit tests), and
   `cargo test -p pstore-server --test it` (420). The only edit to existing tests is
   `rows_remaining: false` in three literal `Outcome` values in the engine's `applied`
   tests.

Gates, in a cloud container:
- `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings`
  passed.
- ⚠️ Coverage: OBSERVED-NOT locally; llvm-cov in this container reports mismatched data
  and drops counts (see [M136](../M136/VERIFIED.md)). The floor is CI's run on push.
- NOT-RUN here: `scripts/mutants.sh` (`cargo-mutants` is not installed in this container).
