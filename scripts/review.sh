#!/usr/bin/env bash
# Build a review packet, and REFUSE THE ROUND once the budget is spent.
#
# ⚠️ The budget must interrupt the BEHAVIOUR, not just the outcome. In the project
# this was distilled from, the cap refused the COMMIT while nothing refused the next
# ROUND -- so an author could spend pairs of agents indefinitely, and each round felt
# justified because it found something. It always finds something: reviewers verify by
# mutation and the marginal round is never empty. What decays is SEVERITY. One task
# there reached ELEVEN rounds, with rounds 9-11 fixing minors that never blocked.
#
#   review.sh context <tag>   print the packet for the staged diff
#   review.sh rounds  <tag>   print how many rounds this tag has already had
set -euo pipefail
cd "$(dirname "$0")/.."
CMD=${1:-context}
TAG=${2:-$(git rev-parse --abbrev-ref HEAD)}
DIR=.harness/review
BUDGET=${REVIEW_ROUND_BUDGET:-4}
mkdir -p "$DIR"
. scripts/lib/py.sh

# ⚠️ The round counter is the mechanism, so it is tested (tests/harness.bats-style
# check in scripts/selftest-review.sh). In the source project the counter resolved the
# round from a sha that only existed AFTER the packet was made, so it printed 1 on
# every round for the life of the gate and the cap never fired once.
# `|| true`: with `set -e -o pipefail`, ls finding nothing aborts the caller --
# which would make round 1 crash rather than count 0. Caught by selftest-review.sh.
rounds() { local n; n=$(ls "$DIR/${1}.r"*.packet 2>/dev/null | wc -l || true); echo "${n// /}"; }

# ⚠️ The counter is keyed to the HEAD the review STARTED on, and a commit ends a review.
# Without this the count was keyed on the branch name alone, so on a long-lived `main` it
# accumulated rounds across unrelated changes -- observed reporting "round 4 of hard budget
# 4" for a change whose review had not started, which REFUSES a change that has had no
# rounds at all. Backlog row 22.
#
# ⚠️ Deliberately NOT keyed on the staged tree, the other candidate: the staged tree is what
# CHANGES between rounds, so keying on it would reset the budget on every fix and delete the
# mechanism. A HEAD that moves mid-review for an unrelated reason -- an amend, a rebase --
# costs an extra round, which is the failure worth having.
BASE="$DIR/${TAG}.base"
HEAD_NOW=$(git rev-parse HEAD 2>/dev/null || echo none)
if [ -f "$BASE" ] && [ "$(cat "$BASE")" != "$HEAD_NOW" ]; then
  rm -f "$DIR/${TAG}.r"*.packet "$DIR/${TAG}.r"*.tree "$DIR/${TAG}.r"*.sha "$BASE"
fi

case "$CMD" in
  rounds) rounds "$TAG"; exit 0 ;;
  context) ;;
  *) echo "usage: $0 {context|rounds} [tag]" >&2; exit 2 ;;
esac

N=$(( $(rounds "$TAG") + 1 ))
if [ "$N" -gt "$BUDGET" ]; then
  cat >&2 <<MSG
!!! Round $N for '$TAG' exceeds the budget of $BUDGET.
!!!
!!! The remedy is SPLIT, not another round. A change that cannot survive two
!!! rounds is too big. If the last rounds have been finding prose rather than
!!! defects, the honest move is to DELETE the prose that keeps being wrong
!!! rather than correct it again -- each correction is new surface that buys
!!! the next round.
!!!
!!! Deliberate override: REVIEW_ROUND_BUDGET=$N $0 context $TAG
MSG
  exit 1
fi

echo "$HEAD_NOW" > "$BASE"
# The staged tree IS what this round reviews, so it is what the next round diffs against.
# ⚠️ Recorded before the packet, unlike the sha it replaces, which was written after.
TREE=$(git write-tree 2>/dev/null || echo "")
PACKET="$DIR/${TAG}.r${N}.packet"
{
  echo "=== ROUND $N of 2 (hard budget $BUDGET) for '$TAG' ==="
  echo "Round one finds, round two verifies. A blocking finding in round two means"
  echo "the change is TOO BIG -- it is split, not reviewed a third time."
  echo
  echo "Only 'blocking' and 'major' stop a commit. A 'pass' carrying 'minor' findings"
  echo "LANDS; minors go in the commit body or the backlog. Fixing a minor is permitted"
  echo "and usually wrong, because the new round's surface is the prose the fix added."
  echo "AN EMPTY FINDINGS LIST IS A VALID AND EXPECTED OUTCOME."
  echo
  echo "You are NOT given the author's reasoning. That is deliberate."
  echo
  echo "=== GATES THAT ALREADY PASSED (do not re-check these) ==="
  for g in "cargo fmt --check" "cargo clippy --all-targets -- -D warnings" \
           "cargo test -q" "./scripts/check-links.sh" "py ./scripts/build-index.py --check"; do
    if eval "$g" >/dev/null 2>&1; then echo "  PASS  $g"; else echo "  FAIL  $g   <-- fix before reviewing"; fi
  done
  echo
  if [ "$N" -eq 1 ]; then
    echo "=== STAGED DIFF ==="
    git diff --cached
  else
    # ⚠️ A verify round gets the DELTA, not the whole diff again. Re-sending the full
    # diff is what made packets in the source project 9,977 lines each, six rounds
    # running -- roughly 700k tokens on a single task.
    # ⚠️ Tree to tree, not sha to worktree. The old form diffed the worktree against the
    # sha recorded AFTER the previous packet -- which on a moved HEAD is an already-committed
    # diff, and on an unmoved one is the whole diff again rather than the delta it claims.
    PREV="$DIR/${TAG}.r$((N-1)).tree"
    echo "=== DELTA SINCE ROUND $((N-1)) (the full diff was reviewed then) ==="
    if [ -s "$PREV" ] && [ -n "$TREE" ]; then git diff "$(cat "$PREV")" "$TREE"; else git diff --cached; fi
  fi
} > "$PACKET"
echo "$TREE" > "$DIR/${TAG}.r${N}.tree"
cat "$PACKET"
echo "--- packet: $PACKET ($(wc -l < "$PACKET") lines) ---" >&2
