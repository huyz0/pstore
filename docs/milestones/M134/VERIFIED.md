# M134 verification

The spec was reviewed in one round, recorded in the [spec](SPEC.md).

How to run the tests: `cargo test -p pstore-engine --test it idempotency::`.

Red runs:
- Criteria 1 and 2 were run against the parent's waiter before the change. Both answered
  `Unavailable`.
- Criterion 3 is a guard, green on the parent, as the spec says.
- Criterion 4's test was written after the change. Its red is the mutation "the unknown
  case answers `Unavailable`", which restores the parent's behaviour; the test failed
  under it.

The mutations were applied one at a time by a script and then reverted. No `cargo mutants`
run is claimed for them.

1. `a_fallen_loser_is_replayed_not_applied`: green after. Killed by "the unknown case
   answers `Unavailable`" and "the unknown case is always `Partial`".
2. `a_fallen_loser_with_no_record_is_partial`: green after. Killed by "the unknown case
   answers `Unavailable`" and "no winner named answers `Replayed(None)`".
3. `a_fallen_winner_is_unavailable`: green after. Killed by "a named winner is always
   replayed" and "the unknown case is always `Partial`".
4. `a_fallen_sole_winner_is_partial`: green after. Killed by "the unknown case answers
   `Unavailable`" and "no winner named answers `Replayed(None)`".
5. Every M132 test passed unchanged: the engine's `idempotency::` module (16 tests, the
   four above among them) and the server's (5 tests). The server's
   `a_refusal_whose_window_fell_is_partial` checks the `idempotency_partial` code, not the
   reworded message.

Gates, in a cloud container:
- `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings`
  passed. The engine's `idempotency::` and `applied` tests and the server's
  `idempotency::` tests passed.
- ⚠️ Coverage: OBSERVED-NOT locally. After the tests moved into one binary per crate,
  `scripts/coverage.sh` in this container reads `pstore-engine/src/lib.rs` at about 62%
  regions, and the workspace at 89%, with every test passing. `cargo llvm-cov -p
  pstore-engine` on unchanged `main` reads the crate at 64% too, so the fault is in the
  local measurement, not in this change. CI's coverage job read `main` above both floors
  on the commits just before this one. The floor for this change is CI's run on push.
- NOT-RUN here: `scripts/mutants.sh` (`cargo-mutants` is not installed in this container).
