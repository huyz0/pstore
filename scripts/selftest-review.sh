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
