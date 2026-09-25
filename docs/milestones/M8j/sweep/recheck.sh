#!/usr/bin/env bash
# portable: no -- M8j: re-check the index-only candidates against the WHOLE workspace.
# Each shard's MISSED list overstates the survivors (other crates' tests can catch an index
# mutant); this runs exactly those names with the workspace's tests and commits the result.
set -euo pipefail
cd "$(dirname "$0")/../../../.."
dir=docs/milestones/M8j/sweep
out=$dir/workspace-recheck.txt
[[ -f "$out" ]] && { echo "already recorded"; exit 0; }
names=$(grep -hE '^MISSED' "$dir"/shard-*.txt | sed 's/^MISSED *//')
count=$(printf '%s\n' "$names" | grep -c .)
# One alternation, because cargo-mutants ORs repeated `-F` filters.
re="^($(printf '%s\n' "$names" | sed 's/[][\\.^$*+?(){}|/]/\\&/g' | paste -sd'|'))\$"
log=$(mktemp)
./scripts/mutants.sh --check "$re" --file 'crates/pstore-index/src/*.rs' >"$log" 2>&1 || true
{
    echo "# $count index-only MISSED names, re-run with the workspace tests, on $(git rev-parse --short HEAD)"
    grep -E '^(MISSED|TIMEOUT) ' "$log" | sed 's/ in [0-9].*$//' || true
    grep -E 'mutants tested in|Found [0-9]+ mutants' "$log" || echo "NO SUMMARY -- did not finish"
} >"$out"
git add "$out"
git commit -q -m "M8j measurement: the index-only survivors, re-checked against the workspace" \
    -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_018mFZLsYj2MhCDsPW7yv9Fv" -- "$out"
for d in 2 4 8 16; do git push -q origin main-ujunsi && break || sleep "$d"; done
