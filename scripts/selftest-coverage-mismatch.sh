#!/usr/bin/env bash
# gate-design step 2: make it fail first. When llvm-cov cannot match a function's counts to
# the binary it reports from, it drops them and says only "N functions have mismatched data":
# a delivery session read 89% where CI read 95%, from the same tree. `coverage.sh` must
# refuse that report rather than print a number. A fake `cargo` stands in for llvm-cov.
set -euo pipefail
cd "$(dirname "$0")/.."
fake=$(mktemp -d)
trap 'rm -rf "$fake"' EXIT
real=$(command -v cargo)
cat > "$fake/cargo" <<FAKE
#!/usr/bin/env bash
if [ "\${1:-}" = "llvm-cov" ]; then
  [ -n "\${FAKE_MISMATCH:-}" ] && echo "warning: 7 functions have mismatched data" >&2
  echo "TOTAL 100 0 100.00%"
  exit "\${FAKE_RC:-0}"
fi
exec "$real" "\$@"
FAKE
chmod +x "$fake/cargo"
# The empty tree as BASE: every crate changed, so `--changed` reaches llvm-cov, in a shallow
# CI checkout too.
empty=4b825dc642cb6eb9a060e54bf8d69288fbee4904
for mode in "" "--fast" "--changed $empty"; do
  # Refused for that reason, named: any other failure would pass a bare exit-status check.
  # shellcheck disable=SC2086 # the mode is words
  if out=$(FAKE_MISMATCH=1 PATH="$fake:$PATH" ./scripts/coverage.sh $mode 2>&1) \
      || ! printf '%s' "$out" | grep -q "FAIL coverage: llvm-cov dropped"; then
    echo "FAIL coverage.sh ${mode:-(full)} did not refuse a run whose counts did not match"
    exit 1
  fi
done
# ... and only that: a clean report passes, and a failing one keeps llvm-cov's status.
PATH="$fake:$PATH" ./scripts/coverage.sh >/dev/null 2>&1 \
  || { echo "FAIL coverage.sh refused a report with every count matched"; exit 1; }
rc=0
FAKE_RC=3 PATH="$fake:$PATH" ./scripts/coverage.sh >/dev/null 2>&1 || rc=$?
[ "$rc" = 3 ] || { echo "FAIL coverage.sh exited $rc, not llvm-cov's 3"; exit 1; }
echo "ok coverage.sh refuses a report with mismatched data"
