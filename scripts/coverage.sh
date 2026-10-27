#!/usr/bin/env bash
# Coverage on the crates that actually ship.
#
# ⚠️ Closes M1.12. The question was whether `pstore-testkit` belongs in the coverage
# measure, and it was open because it looked like a judgement call. It is not: a crate that
# no other crate depends on outside `[dev-dependencies]` does not ship, and that is a
# predicate over files in the tree. Rung 3 of the gate-design ladder -- so it is computed
# here rather than remembered by whoever runs the command, and a new test-only crate is
# excluded automatically instead of being argued about.
#
#   scripts/coverage.sh                          summary for shipped crates
#   scripts/coverage.sh --all                    include test-only crates, for comparison
#   scripts/coverage.sh --fail-under-regions 95  the gate
#   scripts/coverage.sh --fast [args]            the same measure, reusing the last build
#   scripts/coverage.sh --changed [BASE] [args]  only the crates changed since BASE
#                                                (default: the merge-base with origin/main)
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/lib/py.sh

# Which members SHIP, and therefore belong inside the floor.
#
# ⚠️ **A crate declares this; it is not inferred from the dependency graph.** The first
# version computed it — "a crate nothing depends on outside `[dev-dependencies]` does not
# ship" — which is sound reasoning and was wrong about the most important crate in the
# workspace. `pstore-engine` holds the commit protocol and Invariant I1, and it is named only
# as a **dev**-dependency of `pstore-index`, because the composition root that will depend on
# it does not exist yet. So the correctness core sat outside the coverage gate, and the line
# saying so was printed on every run and read by nobody. Found in M7a.
#
# Inference gets the default backwards. A new crate that nothing depends on yet is *most*
# likely to be new code that needs the floor, and it was the case the rule silently excluded.
# Declaring it inverts that: a crate is measured unless it says otherwise, and saying
# otherwise is a line in a `Cargo.toml` that shows up in a diff — the same argument
# `unsafe_code = "forbid"` makes for the `unsafe` audit.
#
#     [package.metadata.pstore]
#     ships = false   # test infrastructure: never linked into anything that runs
#
# ⚠️ The derivation -- which crates declare `ships = false`, and why binary entry points are
# composition roots whose coverage is structurally zero -- lives in `scripts/lib/scope.py`,
# shared with `mutants.sh` so the two gates cannot drift apart about what the codebase is
# (M8b, M8d). Derived from `cargo metadata`, so a new binary is covered the day it is added.
metadata=$(cargo metadata --no-deps --format-version 1)
test_only_names=$(printf '%s' "$metadata" | py scripts/lib/scope.py names)
shipped_filter=$(printf '%s' "$metadata" | py scripts/lib/scope.py llvm-cov)

if [[ "${1:-}" == "--all" ]]; then
    echo "# every workspace member, including test-only crates"
    exec cargo llvm-cov --workspace --summary-only
fi

# ⚠️ **The two fast modes are for the inner loop; the floor is the full run, in CI.** Each full
# run took about 20 minutes on a 4-core container (M131), because `cargo llvm-cov` cleans the
# workspace's build before every run, and the agent sat idle through it.
#
# `--fast` keeps the instrumented build and deletes only the old profile counts, so cargo
# rebuilds what changed and every count is from this run. CI keeps the clean build.
#
# `--changed` also runs only the tests of the shipping crates the change touches, and
# reports only those crates. ⚠️ It can read LOW, never high: a crate's code exercised only by
# another crate's tests (pstore-blob's decorators by pstore-testkit's conformance suite) is
# counted uncovered here. So a red `--changed` is a question and a green one is a hint; the
# 95% floors are only ever decided by the full run.
#
# Measured after the test-binary merge, `provisional` (a 4-core cloud container): full 603 s,
# `--fast` after touching pstore-engine 556 s, `--changed` on pstore-engine 242 s. Running the
# instrumented tests is most of the cost now, so `--fast` saves little; `--changed` saves by
# running fewer of them, and pstore-engine is the slowest crate to pick.
# ⚠️ **A report with dropped counts is refused, not printed.** When llvm-cov cannot match a
# function's counts to the binary it reports from, it drops them and says only "N functions
# have mismatched data" -- and the number it prints is low by whatever it dropped. A delivery
# session read 89% (pstore-engine's lib.rs at 59%) where CI read 95%, from the same tree, with
# 190 such functions. `cargo llvm-cov clean --workspace` and a rerun is the remedy.
report() {
    local log rc
    log=$(mktemp)
    set +e
    cargo llvm-cov "$@" 2>&1 | tee "$log"
    rc=${PIPESTATUS[0]}
    set -e
    if grep -q "functions have mismatched data" "$log"; then
        rm -f "$log"
        echo "FAIL coverage: llvm-cov dropped the counts of functions with mismatched data," \
            "so this number is low by an unknown amount. Run \`cargo llvm-cov clean" \
            "--workspace\` and measure again." >&2
        exit 1
    fi
    rm -f "$log"
    exit "$rc"
}

fast=()
if [[ "${1:-}" == "--fast" || "${1:-}" == "--changed" ]]; then
    fast=(--no-clean)
    cargo llvm-cov clean --profraw-only
fi
if [[ "${1:-}" == "--changed" ]]; then
    shift
    base=""
    if [[ $# -gt 0 && "$1" != -* ]]; then base=$1; shift; fi
    if [[ -z "$base" ]]; then
        # No silent fallback: a base of HEAD would count only uncommitted work and report a
        # committed change as "nothing to measure".
        base=$(git merge-base HEAD origin/main 2>/dev/null) \
            || { echo "# --changed: no origin/main to diff against; pass BASE" >&2; exit 2; }
    fi
    # Committed since BASE, staged, unstaged and untracked: the change as it stands.
    paths=$( { git diff --name-only "$base"; git ls-files --others --exclude-standard; } | sort -u)
    # shellcheck disable=SC2086 # the paths are git's, one word each
    picked=$(printf '%s' "$metadata" | py scripts/lib/scope.py changed $paths)
    crates=$(printf '%s\n' "$picked" | sed -n 1p)
    ignore=$(printf '%s\n' "$picked" | sed -n 2p)
    if [[ -z "$crates" ]]; then
        echo "# --changed: no shipping crate changed since $base; nothing to measure"
        exit 0
    fi
    pkgs=()
    for c in $crates; do pkgs+=(-p "$c"); done
    echo "# --changed since $(git rev-parse --short "$base"): $crates -- their own tests only;" \
        "a proxy, the floor is the full run"
    # An empty regex would ignore every file, as the full run's guard below says.
    if [[ -z "$ignore" ]]; then
        report ${fast[@]+"${fast[@]}"} "${pkgs[@]}" --summary-only "$@"
    fi
    report ${fast[@]+"${fast[@]}"} "${pkgs[@]}" --summary-only \
        --ignore-filename-regex "$ignore" "$@"
fi
if [[ "${1:-}" == "--fast" ]]; then shift; fi

# Anything else is passed straight through, so the gate is the same command a developer
# runs with one more flag -- not a second, differently-scoped measurement.
extra=("$@")

if [[ -z "$shipped_filter" ]]; then
    echo "# no test-only crates; measuring the whole workspace"
    report ${fast[@]+"${fast[@]}"} --workspace --summary-only "${extra[@]}"
fi

echo "# excluding test-only crates and binary entry points: $test_only_names"
report ${fast[@]+"${fast[@]}"} --workspace --summary-only \
    --ignore-filename-regex "$shipped_filter" "${extra[@]}"
