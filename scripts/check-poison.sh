#!/usr/bin/env bash
# Shipping code must RECOVER a poisoned lock, never drop the `Result` on the floor.
#
# ⚠️ **This is a gate because fixing one instance leaves the next one to be found the same
# expensive way.** `pstore-catalog`'s appender read a poisoned lock as "not seen" and then
# skipped the insert that remembers what it wrote -- so it lost its dedupe for that tenant for
# the life of the process, and the lifecycle-rate append C-12's bounded write depends on
# became a commit-rate one. Nothing reported it; a coverage number found it, two milestones
# later. `gate-design`'s ladder: a rule that can be stated as a predicate over files must not
# be left to someone remembering.
#
# The idiom the tree already uses, in six modules:
#
#   .lock().unwrap_or_else(std::sync::PoisonError::into_inner)
#
# ⚠️ **Shipping code only.** A decorator in a test that drops a poisoned lock loses a
# recording, not a bounded write, and `tests/` is full of them deliberately.
#
# ⚠️ **It is a grep, and greps go stale.** It catches the forms below and does not pretend to
# catch every possible spelling. `--selftest` proves it catches these.
set -euo pipefail
cd "$(dirname "$0")/.."

# Each pattern is one way to turn a poisoned lock into silently-degraded behaviour.
PATTERNS=(
    'is_ok_and\(\|[^)]*\)[[:space:]]*$'   # `self.x.lock().is_ok_and(|g| ...)`
    'if let Ok\(.*\) = .*\.lock\(\)'
    'while let Ok\(.*\) = .*\.lock\(\)'
    '\.lock\(\)\.ok\(\)'
    '\.lock\(\)\.unwrap_or_default\(\)'
)

scan() {
    local root=$1 found=0
    while IFS= read -r f; do
        # Only lines in the same statement as a `.lock()` matter for the first pattern, so it
        # is applied to a two-line window; the rest are self-contained.
        if grep -nE 'is_ok_and' "$f" | grep -q . && grep -nB2 'is_ok_and' "$f" | grep -q '\.lock()'; then
            grep -nB2 'is_ok_and' "$f" | grep -q '\.lock()' && { echo "  $f: a poisoned lock read through is_ok_and"; found=1; }
        fi
        for p in "${PATTERNS[@]:1}"; do
            if grep -nE "$p" "$f" >/dev/null 2>&1; then
                grep -nE "$p" "$f" | while IFS= read -r hit; do echo "  $f:$hit"; done
                found=1
            fi
        done
    done < <(find "$root" -path '*/src/*' -name '*.rs' 2>/dev/null)
    return "$found"
}

if [ "${1:-}" = "--selftest" ]; then
    # ⚠️ A gate that cannot fail reports success while checking nothing -- which `gates.sh`'s
    # own header records catching in itself on its first run. So the gate is run against a
    # tree that MUST fail it.
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    mkdir -p "$tmp/crates/x/src"
    cat > "$tmp/crates/x/src/bad.rs" <<'RS'
fn f(&self) {
    if let Ok(mut g) = self.seen.lock() {
        g.insert(1, 2);
    }
}
RS
    if scan "$tmp/crates" >/dev/null 2>&1; then
        echo "FAIL check-poison.sh: it passed a file that drops a poisoned lock" >&2
        exit 1
    fi
    mkdir -p "$tmp/good/crates/x/src"
    cat > "$tmp/good/crates/x/src/ok.rs" <<'RS'
fn f(&self) {
    self.seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(1, 2);
}
RS
    if ! scan "$tmp/good/crates" >/dev/null 2>&1; then
        echo "FAIL check-poison.sh: it refused the idiom the tree actually uses" >&2
        exit 1
    fi
    echo "ok check-poison.sh refuses the degrading form and accepts the idiom"
    exit 0
fi

if ! scan crates; then
    cat >&2 <<'MSG'
FAIL: shipping code drops a poisoned lock instead of recovering it.

A dropped `Result` from `.lock()` is not "handle the error" -- it is silently skipping the
work. `pstore-catalog`'s appender skipped remembering what it had written, and lost its
dedupe for that tenant for the life of the process.

Use the idiom the rest of the tree uses:

    .lock().unwrap_or_else(std::sync::PoisonError::into_inner)
MSG
    exit 1
fi
echo "ok no shipping code drops a poisoned lock"
