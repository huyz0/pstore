#!/usr/bin/env bash
# gate-design step 2: make it fail first. A gate never observed failing is not known to gate
# anything -- and this one's whole job is to refuse a sentence that reads perfectly well.
set -euo pipefail
cd "$(dirname "$0")/.."
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT

. scripts/lib/py.sh
pass() { py ./scripts/check-slos.py "$1" >/dev/null 2>&1; }

# A row that names a real gate passes.
cat > "$T/good.md" <<'MD'
| Objective | Value | Status | Gate |
|---|---|---|---|
| Zero LISTs on every serving path | 0 | enforced | `no_endpoint_lists` |
| p99 query latency | | blocked | M0b: TTFB percentiles from inside the cloud |
MD
pass "$T/good.md" || { echo "FAIL a well-formed file was refused"; exit 1; }

# ⚠️ The failure this gate exists for: `enforced`, and the gate does not exist. It reads
# exactly like the row above.
cat > "$T/ghost.md" <<'MD'
| Objective | Value | Status | Gate |
|---|---|---|---|
| Zero LISTs on every serving path | 0 | enforced | `a_test_that_was_deleted_two_milestones_ago` |
MD
pass "$T/ghost.md" && { echo "FAIL an enforced row naming a gate that does not exist passed"; exit 1; }

# A deleted script, which is the same failure wearing a path.
cat > "$T/script.md" <<'MD'
| Objective | Value | Status | Gate |
|---|---|---|---|
| Recall stays above its floor | 0.90 | enforced | `scripts/recall-that-is-gone.sh` |
MD
pass "$T/script.md" && { echo "FAIL an enforced row naming a missing script passed"; exit 1; }

# A blocked row that names no blocker is a wish.
cat > "$T/wish.md" <<'MD'
| Objective | Value | Status | Blocked on |
|---|---|---|---|
| p99 query latency | | blocked | - |
MD
pass "$T/wish.md" && { echo "FAIL a blocked row with no blocker passed"; exit 1; }

# A status outside the two is the third status this file exists to forbid.
cat > "$T/aspiration.md" <<'MD'
| Objective | Value | Status | Gate |
|---|---|---|---|
| p99 query latency | 100ms | aspirational | we are working on it |
MD
pass "$T/aspiration.md" && { echo "FAIL a third status passed"; exit 1; }

echo "ok check-slos refuses a ghost gate, a missing script, a wish, and a third status"
