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

# Every workspace member, and every member named as a NON-dev dependency by any member.
scope=$(cargo metadata --no-deps --format-version 1 | python3 -c '
import json, sys
md = json.load(sys.stdin)
names = {p["name"] for p in md["packages"]}
# A dependency with kind "dev" carries kind == "dev"; a normal one carries null.
runtime = set()
for p in md["packages"]:
    for d in p["dependencies"]:
        if d["name"] in names and d.get("kind") != "dev":
            runtime.add(d["name"])
# A member is shipped if anything depends on it at runtime, or if nothing depends on it at
# all but it is not test infrastructure -- i.e. it is a root. Roots are shipped: they are
# what a binary would link.
depended = {d["name"] for p in md["packages"] for d in p["dependencies"] if d["name"] in names}
test_only = sorted(n for n in names if n in depended and n not in runtime)
# Two lines: the human-readable names, then the regex llvm-cov needs (paths use either
# separator depending on where the file came from).
print(" ".join(test_only))
print("|".join(t.replace("-", "[-_]") for t in test_only))
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

echo "# excluding test-only crates: $test_only_names"
exec cargo llvm-cov --workspace --summary-only \
    --ignore-filename-regex "$shipped_filter" "${extra[@]}"
