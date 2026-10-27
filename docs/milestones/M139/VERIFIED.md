# M139 verification

The spec was reviewed in one round, recorded in the [spec](SPEC.md); the code in one,
pass with two minors, both taken.

How to run the tests:
- engine: `cargo test -p pstore-engine --test it filter_rows::`;
- server: `cargo test -p pstore-server --test it -- allow_partial:: applied_writes::`.

Red runs: the engine's `ByFilter` and `*_by_filter_bounded` calls landed first as stubs
ignoring the bound, and the server's level-5 setter landed unread. Against the stubs,
criteria 1, 3, 4 and 5 failed. Criterion 2's first case passed (the engine's bound
already refused it) and its fold-order case was added after a mutation survived; its red
is that mutation. Criterion 7 passed on the stub, because the write was refused for its
by-filter operation; its red is the "10,000 named ids served" mutation.

The mutations were applied one at a time by a script and then reverted, running the
engine's `filter_rows::` tests and the server's `allow_partial::` and `applied_writes::`
tests. All ten were killed, one after a test was added (below). No `cargo mutants` run is
claimed for them. The spec's "ids not recorded for by-filter changes" was run by hand
after code review: ids and counts are recorded by one call (`Tally::changed`), and
removing it in `by_filter` failed five `allow_partial::` tests, criteria 3, 4 and 5 among
them.

1. `an_operations_own_bound_applies`: green after, the fold keeping 2 rows. Killed by
   "`$rows` ignored", "the larger bound wins in the scan" and "the scan retains by the
   engine's bound".
2. `the_smaller_bound_wins`: green after. "The larger bound wins in fold order" survived
   the first version, whose case the scan refused anyway; a fold-order case was added,
   and it kills it.
3. `each_pass_lists_the_ids_it_deleted`: green after. Killed by "refused at level 5".
4. `a_patch_by_filter_lists_what_it_patched`: green after. Killed by "refused at level 5".
5. `the_named_ids_lower_the_bound`: green after. Killed by "the bound is 10,000", "the
   bound is one more" and "the bound is not passed to the engine".
6. `affected_ids_refusals_cost_nothing` (M130, unchanged): green after. Killed by "served
   below level 5".
7. `naming_every_id_leaves_no_bound`: green after, and the refusal made no blob request
   (added in code review). Killed by "10,000 named ids served".
8. `cargo test -p pstore-engine` passed after (535 integration tests, 35 unit tests), and
   `cargo test -p pstore-server --test it` (424), both before criterion 2's fold-order
   case was added; `filter_rows::` passed again after it. No existing test was edited.

Gates, in a cloud container:
- `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings`
  passed.
- ⚠️ Coverage: OBSERVED-NOT locally; llvm-cov in this container drops counts (see
  [M136](../M136/VERIFIED.md)). The floor is CI's run on push.
- NOT-RUN here: `scripts/mutants.sh` (`cargo-mutants` is not installed in this container).
