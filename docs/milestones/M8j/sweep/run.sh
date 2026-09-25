#!/usr/bin/env bash
# portable: no -- a resumable, sharded measurement for M8j; Linux container only.
#
# Measures `pstore-index`'s mutants against pstore-index's OWN tests, one shard at a time,
# committing and pushing each shard's result before starting the next. Why:
#  - index-only tests can only OVERSTATE the misses (anything they catch, the workspace
#    catches too), and cost a fraction of a workspace run per mutant;
#  - the session container restarts every few hours and wipes everything not pushed, so a
#    single 3-hour run never finished. A shard already recorded here is skipped.
set -euo pipefail
cd "$(dirname "$0")/../../../.."
dir=docs/milestones/M8j/sweep
n=8
for k in $(seq 0 $((n - 1))); do
    out="$dir/shard-$k-of-$n.txt"
    [[ -f "$out" ]] && continue
    log=$(mktemp)
    ./scripts/mutants.sh --shard "$k/$n" --file 'crates/pstore-index/src/*.rs' -F pstore-index \
        --test-workspace=false --test-package pstore-index >"$log" 2>&1 || true
    {
        echo "# shard $k/$n of pstore-index, index-only tests, on $(git rev-parse --short HEAD)"
        grep -E '^(MISSED|TIMEOUT) ' "$log" | sed 's/ in [0-9].*$//' || true
        grep -E 'mutants tested in' "$log" || echo "NO SUMMARY -- the shard did not finish"
    } >"$out"
    git add "$out"
    git commit -q -m "M8j measurement: pstore-index shard $k/$n" \
        -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_018mFZLsYj2MhCDsPW7yv9Fv" -- "$out"
    for d in 2 4 8 16; do git push -q -u origin main-ujunsi && break || sleep "$d"; done
    rm -f "$log"
done
echo "all shards recorded"
