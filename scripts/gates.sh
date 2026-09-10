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

# ⚠️ First, because the others are what it protects: without the build ceilings a
# `cargo test --workspace` on an uncapped WSL2 VM is how this machine died, twice.
run "scripts/check-dev-env.sh"     ./scripts/check-dev-env.sh
run "cargo fmt --check"            cargo fmt --all --check
run "cargo clippy -D warnings"     cargo clippy --all-targets --all-features -- -D warnings
run "cargo test"                   cargo test --workspace --all-features --quiet
run "scripts/check-poison.sh"      ./scripts/check-poison.sh
run "scripts/check-links.sh"       ./scripts/check-links.sh
run "scripts/build-index.py --check" ./scripts/build-index.py --check
run "scripts/check-verified.py"    ./scripts/check-verified.py
run "scripts/selftest-review.sh"   ./scripts/selftest-review.sh
run "scripts/selftest-check-verified.sh" ./scripts/selftest-check-verified.sh
run "scripts/check-poison.sh --selftest" ./scripts/check-poison.sh --selftest
echo "all gates green"
