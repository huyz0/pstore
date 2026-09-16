#!/usr/bin/env bash
# gate-design step 2: make it fail first. A gate never observed failing is not
# known to gate anything -- and the round counter is precisely the thing that
# silently broke in the project this was distilled from.
set -euo pipefail
cd "$(dirname "$0")/.."
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
export REVIEW_ROUND_BUDGET=2
TAG="selftest-$$"
DIR=.harness/review; mkdir -p "$DIR"
cleanup() { rm -f "$DIR/${TAG}."*; }
trap 'cleanup; rm -rf "$T"' EXIT

[ "$(./scripts/review.sh rounds "$TAG")" = "0" ] || { echo "FAIL fresh tag should be round 0"; exit 1; }
./scripts/review.sh context "$TAG" >/dev/null
[ "$(./scripts/review.sh rounds "$TAG")" = "1" ] || { echo "FAIL counter did not advance"; exit 1; }
./scripts/review.sh context "$TAG" >/dev/null
[ "$(./scripts/review.sh rounds "$TAG")" = "2" ] || { echo "FAIL counter did not advance twice"; exit 1; }
if ./scripts/review.sh context "$TAG" >/dev/null 2>&1; then
  echo "FAIL round 3 should have been refused at budget 2"; exit 1
fi
REVIEW_ROUND_BUDGET=3 ./scripts/review.sh context "$TAG" >/dev/null 2>&1 \
  || { echo "FAIL explicit override should be honoured"; exit 1; }
echo "ok round counter advances, budget refuses the round, override works"

# ⚠️ Everything below runs in a SCRATCH repository, because the two properties being
# checked are about HEAD moving and about the staged tree -- neither of which may be
# exercised by committing in the repo under review. `review.sh` resolves its root with
# `cd $(dirname $0)/..`, so a copy of it in $R/scripts makes $R that root.
R=$T/repo; mkdir -p "$R/scripts"; cp scripts/review.sh "$R/scripts/review.sh"
git -C "$R" init -q
git -C "$R" config user.email selftest@pstore.invalid
git -C "$R" config user.name selftest
echo base > "$R/a.txt"; echo base > "$R/b.txt"
git -C "$R" add -A; git -C "$R" commit -qm base
scratch() { REVIEW_ROUND_BUDGET=4 "$R/scripts/review.sh" "$@" scratch; }

# Criterion 1: a commit ENDS a review, so the rounds it spent do not bind the next change.
echo r1 > "$R/a.txt"; git -C "$R" add a.txt
scratch context >/dev/null
scratch context >/dev/null
[ "$(scratch rounds)" = "2" ] || { echo "FAIL scratch counter did not reach 2"; exit 1; }
git -C "$R" commit -qm "the change those rounds reviewed"
[ "$(scratch rounds)" = "0" ] || { echo "FAIL rounds survived the commit they reviewed"; exit 1; }

# Criterion 2: round 2 gets the DELTA against round 1's staged tree, not the worktree
# against a sha. With HEAD unmoved these are indistinguishable unless the delta is a
# tree-to-tree diff: a.txt is staged before round 1 and untouched after it.
echo r2a > "$R/a.txt"; git -C "$R" add a.txt
scratch context >/dev/null
P1=$R/.harness/review/scratch.r1.packet
grep -q 'a\.txt' "$P1" || { echo "FAIL round 1 packet lacks the staged file"; exit 1; }
echo r2b > "$R/b.txt"; git -C "$R" add b.txt
scratch context >/dev/null
P2=$R/.harness/review/scratch.r2.packet
grep -q 'b\.txt' "$P2" || { echo "FAIL round 2 delta lacks the file edited between rounds"; exit 1; }
if grep -q 'a\.txt' "$P2"; then
  echo "FAIL round 2 re-sent a file unchanged since round 1 -- that is the whole diff, not a delta"; exit 1
fi
echo "ok rounds reset when HEAD moves, and round 2 is a tree-to-tree delta"
