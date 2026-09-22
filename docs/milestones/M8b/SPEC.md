# M8b — the line floor measures what the region floor measures

**Serves:** **D-110** (95% line coverage) and **D-111**'s premise that a coverage number means
something. No new ID.

## Delta

CI's `coverage` job ran two floors over **two different scopes**:

- `cargo llvm-cov --all-features --workspace --fail-under-lines 95` — every file, including
  binary entry points and the test-only `pstore-testkit`;
- `./scripts/coverage.sh --fail-under-regions 95` — the crates that ship, with entry points
  excluded, by a rule derived from `cargo metadata` and argued in that script's header:
  `main.rs` wires, `lib.rs` decides, and `cargo test` cannot reach a composition root.

The first has failed since `pstore-server/src/main.rs` and `pstore-node/src/main.rs` grew:
measured at **93.00%** lines, with those two files at 0% (363 lines) and the job red on
`6abdfa3`, before M8a. The two floors were disagreeing about what the codebase *is*.

**Changes:** CI's line step becomes
`./scripts/coverage.sh --all-features --fail-under-lines 95 --fail-under-regions 95` — one
measurement, both floors, the scope `coverage.sh` already derives. `coverage.sh` needs no
change: it passes extra flags through. AGENTS.md's D-110 row names the new command.

**Does not change:** either floor (95 and 95), the exclusion rule, or which crates declare
`ships = false`. ⚠️ Two things do change besides the line scope: the `--lcov lcov.info`
output is dropped (nothing in any workflow reads it), and the **region** floor is now measured
with `--all-features` as the line floor was. The only feature that adds is `pstore-blob`'s
`compat`, which gates cloud clients and an example, not project source. The stale comments
on this CI step are corrected: nothing nightly checks per-crate targets, and the test-only
set is declared (`ships = false`), not derived from the dependency graph.

⚠️ **Is this a weakened threshold? The line gate becomes more lenient**, because files leave
its population; the number stays 95. Of the move from 93.00% to 97.07%, the two entry points
account for 363 lines (all missed) and `pstore-testkit` for 816 lines (71 missed). The
authority is D-110's own exclusion policy — an untestable path may be excluded when the
exclusion says why and is reviewed — and `coverage.sh`'s header is that why, already applied
to the region floor. D-110's per-crate table (`pstore-server` wiring ≥90%) is enforced by
nothing today, before or after this. The honest cost: logic left in a `main.rs` is now
invisible to *both* floors, where before it was visible to one that could never pass. That
was already `coverage.sh`'s stated trade — moving logic into the library is the only way to
get credit for it.

## Acceptance criteria

1. The scoped command CI will run passes: line and region floors both at 95.
2. It can still fail: the same command with `--fail-under-lines 98` exits non-zero.
3. The old whole-workspace command fails on the same tree — the red this change answers.
4. `ci.yml` runs no `cargo llvm-cov` directly — `grep -n 'cargo llvm-cov' .github/workflows/ci.yml`
   prints nothing — and `grep -n 'fail-under' .github/workflows/ci.yml` shows both floors at 95.
5. The CI `coverage` job is green on the pushed commit — `NOT-RUN` until pushed.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | — (the measurement, with the exact `--all-features` command) | a scope that silently drops a shipping crate would move the total |
| 2 | control floor above the measured 97.07% | a gate that cannot fail |
| 3 | observed at 93.00% before any edit | — |
| 4 | the grep before the edit shows the raw step | — a one-time check; no gate reruns it |
| 5 | CI on `6abdfa3` and `25089c7`: red at the line step | — |

## RA budget

Unchanged: CI configuration only.

## Risks

- A future binary that grows logic in `main.rs` loses that logic's coverage signal entirely.
  `coverage.sh` names this; nothing here makes it worse than that script already chose.

## Tasks

- **M8b.1** — the CI step, the AGENTS.md row, this ledger.
