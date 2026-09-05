#!/usr/bin/env bash
# gate-design step 2: make it fail first. A gate never observed failing is not known to
# gate anything. Step 3: check the false-refusal path -- a gate that rejects legitimate
# input gets switched off, and then it enforces nothing.
set -euo pipefail
cd "$(dirname "$0")/.."
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
G=./scripts/check-verified.py
REAL=epoch_orders_and_advances   # a test that actually exists in pstore-types
pass=0

SPEC='## Acceptance criteria
1. A write batch issues exactly one PUT.
2. A cold query'"'"'s sequential depth is at most three.'

# fixture <name> <pass|fail> <verified-body|__none__>
fixture() {
  local name=$1 expect=$2 body=$3
  local d="$T/$name"; mkdir -p "$d"
  printf '%s\n' "$SPEC" > "$d/SPEC.md"
  [ "$body" = "__none__" ] || printf '%s\n' "$body" > "$d/VERIFIED.md"
  local got=fail
  $G --self-test-fixture "$d" >/dev/null 2>&1 && got=pass
  [ "$got" = "$expect" ] || { echo "FAIL [$name] expected $expect, got $got"; $G --self-test-fixture "$d" || true; exit 1; }
  pass=$((pass+1))
}

# --- false-refusal path: legitimate input must be ACCEPTED ---
fixture well-formed pass "1. \`$REAL\`. \`cargo test -p pstore-types\`. Mutation verified killed.
2. \`$REAL\` (same command)."
fixture honest-not-run pass "1. \`$REAL\`. \`cargo test\`.
2. NOT-RUN -- the fault-injecting store does not exist yet."
fixture observed-not pass "1. \`$REAL\`. \`cargo test\`.
2. OBSERVED-NOT, resolved by construction: the type makes a fourth hop unrepresentable."
fixture continuation-lines pass "1. \`$REAL\`. \`cargo test\`.
   Mutation verified killed (saturating add removed).
2. \`$REAL\`. \`cargo test\`."
fixture in-progress-no-ledger pass __none__

# --- must FAIL ---
fixture missing-evidence fail "1. \`$REAL\`. \`cargo test\`."
fixture drifted-test-name fail "1. \`$REAL\`. \`cargo test\`.
2. \`epoch_orders_and_advances_renamed_last_week\`. \`cargo test\`."
fixture no-test-no-abstention fail "1. \`$REAL\`. \`cargo test\`.
2. It works, I checked."
fixture evidence-for-ghost-criterion fail "1. \`$REAL\`. \`cargo test\`.
2. \`$REAL\`. \`cargo test\`.
3. \`$REAL\`. A criterion the spec does not contain."

echo "ok $pass cases: rejects missing evidence, drifted test names, unevidenced claims"
echo "   and ghost criteria; accepts NOT-RUN, OBSERVED-NOT and in-progress milestones"
