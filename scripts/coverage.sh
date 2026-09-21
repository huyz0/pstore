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
scope=$(cargo metadata --no-deps --format-version 1 | py -c '
import json, sys
md = json.load(sys.stdin)
test_only = sorted(
    p["name"]
    for p in md["packages"]
    if (p.get("metadata") or {}).get("pstore", {}).get("ships") is False
)

# Binary ENTRY POINTS are composition roots: they read the environment, open connections and
# loop, and nothing outside the operating system can call them. `cargo test` cannot reach one,
# so its coverage is structurally zero and drags a real floor down to a number nobody trusts.
#
# ⚠️ This is an exclusion of WIRING, never of logic, and it is what makes the rule
# "`main.rs` wires, `lib.rs` decides" enforceable rather than advisory: logic left in a
# binary is now INVISIBLE to the gate, so moving it to the library is the only way to get
# credit for testing it. Measured before that split, the decisions in `pstore-node` -- when to
# dial a peer, what to publish, how long to retry a join -- sat in `main` at 0% coverage, and
# two of them were bugs found by a hundred containers instead of by a test.
#
# Derived from `cargo metadata`, so a new binary is covered by the rule the day it is added.
roots = sorted(
    t["src_path"]
    for p in md["packages"]
    for t in p["targets"]
    if t["kind"] == ["bin"]
)
patterns = [t.replace("-", "[-_]") for t in test_only]
patterns += ["/".join(r.rsplit("/", 3)[-3:]) for r in roots]
# Two lines: the human-readable names, then the regex llvm-cov needs (paths use either
# separator depending on where the file came from).
print(" ".join(test_only + [r.rsplit("/", 3)[-1] for r in roots]))
print("|".join(patterns))
')
test_only_names=$(printf '%s\n' "$scope" | sed -n 1p)
shipped_filter=$(printf '%s\n' "$scope" | sed -n 2p)

if [[ "${1:-}" == "--all" ]]; then
    echo "# every workspace member, including test-only crates"
    exec cargo llvm-cov --workspace --summary-only
fi

# Anything else is passed straight through, so the gate is the same command a developer
# runs with one more flag -- not a second, differently-scoped measurement.
extra=("$@")

if [[ -z "$shipped_filter" ]]; then
    echo "# no test-only crates; measuring the whole workspace"
    exec cargo llvm-cov --workspace --summary-only "${extra[@]}"
fi

echo "# excluding test-only crates and binary entry points: $test_only_names"
exec cargo llvm-cov --workspace --summary-only \
    --ignore-filename-regex "$shipped_filter" "${extra[@]}"
