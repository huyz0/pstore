#!/usr/bin/env bash
# Every gate, with the exit status intact.
#
# ⚠️ `cargo clippy ... | tail -1` returns TAIL's status, not clippy's -- a failing gate
# reports green. That happened here on the first run, which is the exact pathology
# gate-design names: a check that cannot fail deterministically reports success while
# checking nothing. `pipefail` is why this script exists.
set -euo pipefail
cd "$(dirname "$0")/.."
run() { printf '%-42s' "$1"; shift; if "$@" >/tmp/pstore-gate.log 2>&1; then echo "PASS"; else echo "FAIL"; tail -25 /tmp/pstore-gate.log; exit 1; fi; }

run "cargo fmt --check"            cargo fmt --all --check
run "cargo clippy -D warnings"     cargo clippy --all-targets --all-features -- -D warnings
run "cargo test"                   cargo test --workspace --all-features --quiet
run "scripts/check-links.sh"       ./scripts/check-links.sh
run "scripts/build-index.py --check" ./scripts/build-index.py --check
run "scripts/check-verified.py"    ./scripts/check-verified.py
run "scripts/selftest-review.sh"   ./scripts/selftest-review.sh
run "scripts/selftest-check-verified.sh" ./scripts/selftest-check-verified.sh
echo "all gates green"
