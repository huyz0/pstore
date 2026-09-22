# M8b — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

All coverage runs are in the Linux dev container, with `cargo-llvm-cov` 0.9.1 and
`RUSTFLAGS="-D warnings"` as CI sets it.

1. **The scoped command passes** —
   `./scripts/coverage.sh --all-features --fail-under-lines 95 --fail-under-regions 95`, the
   exact CI step: exit 0, with **97.07%** lines, 95.11% regions, 96.89% functions, excluding
   `pstore-testkit` and both `main.rs` files. The same numbers without `--all-features`, as
   spec review predicted — `compat` gates no project source.
2. **It can still fail** —
   `./scripts/coverage.sh --all-features --fail-under-lines 98 --fail-under-regions 95` exits **1** at the
   same 97.07%. The region margin is thin (0.11 points), which is the region floor's existing
   state, not this change's.
3. **The old command fails on the same tree** —
   `cargo llvm-cov --all-features --workspace --fail-under-lines 95 --summary-only`: exit 1 at
   **93.00%** lines, with `pstore-node/src/main.rs` (289 lines) and `pstore-server/src/main.rs`
   (74) at 0%, and every test suite passing — the red was the threshold, never a test.
4. **No raw floor left in CI** — `grep -n 'cargo llvm-cov' .github/workflows/ci.yml` printed
   the raw step (line 70) before the edit and nothing after;
   `grep -n 'fail-under' .github/workflows/ci.yml` shows one step with both floors at 95. A
   one-time check: no gate reruns it.
5. NOT-RUN: **the CI `coverage` job on the pushed commit.** Red on `6abdfa3` and `25089c7`, at
   the line step, before this change; this commit is not pushed yet.
