#!/usr/bin/env bash
# gate-design step 2 for `mutants.sh`'s cleanup (M103): a sweep removes what it created, even
# when it is killed, and nothing another sweep created -- older or younger than it.
#
# ⚠️ Why this exists: a sweep copies the whole tree, build directory included, into
# `$TMPDIR/cargo-mutants-*.tmp` -- gigabytes a job -- and a sweep that was killed left every
# copy behind, along with the diff file `mutants.sh` wrote and meant to delete. The script
# `exec`ed cargo-mutants, which discards the shell's EXIT trap, so the diff leaked on EVERY
# run, killed or not. On WSL2 that filled the host drive, and deleting files later does not
# give the space back: the virtual disk does not shrink.
#
# Runs `mutants.sh` in a SCRATCH repository with a fake `cargo` first on PATH, so no sweep
# runs: the fake makes a copy under `$TMPDIR` the way cargo-mutants does, then does what
# `FAKE` says.
# portable: no -- `mutants.sh` is not (it reads cgroups and `nproc`); this runs in CI's Linux
# harness job, never in `gates.sh`.
set -euo pipefail
cd "$(dirname "$0")/.."
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT

R=$T/repo; mkdir -p "$R/scripts/lib" "$T/bin"
cp scripts/mutants.sh "$R/scripts/"; cp scripts/lib/py.sh scripts/lib/scope.py "$R/scripts/lib/"
git -C "$R" init -q
git -C "$R" config user.email selftest@pstore.invalid
git -C "$R" config user.name selftest
echo base > "$R/a.rs"; git -C "$R" add -A; git -C "$R" commit -qm base
echo change > "$R/a.rs"; git -C "$R" commit -qam change

cat > "$T/bin/cargo" <<'EOF'
#!/usr/bin/env bash
case "$1" in
    metadata) echo '{"workspace_root": "/", "packages": []}' ;;
    mutants)
        # As cargo-mutants does: a copy of the tree, build directory included, under $TMPDIR.
        mkdir -p "$TMPDIR/cargo-mutants-repo-XXXX.tmp/target/debug"
        echo built > "$TMPDIR/cargo-mutants-repo-XXXX.tmp/target/debug/lib"
        echo $$ > "$FAKE_PID"
        case "$FAKE" in
            killed) kill -9 $$ ;;
            # Another sweep starts while this one runs, beside it under the same base.
            # Both layouts: a run directory, and a copy straight under the base.
            younger)
                mkdir -p "$BASE/pstore-mutants-run.younger/cargo-mutants-b.tmp"
                mkdir -p "$BASE/cargo-mutants-younger.tmp"
                exit 2 ;;
            hang) sleep 60 ;;
        esac ;;
    *) exit 2 ;;
esac
EOF
chmod +x "$T/bin/cargo"

# One sweep under a fresh base holding `earlier`, a copy older than the sweep, which may be
# another sweep's: its status in $status, its output in $T/out.
sweep() {
    local fake=$1 mbase=$2
    rm -rf "$T/base" "$T/pid"; mkdir -p "$T/base/cargo-mutants-earlier.tmp"
    status=0
    PATH="$T/bin:$PATH" TMPDIR="$T/base" BASE="$T/base" MUTANTS_BASE=$mbase FAKE=$fake \
        FAKE_PID="$T/pid" "$R/scripts/mutants.sh" >"$T/out" 2>&1 || status=$?
}
left() { (cd "$T/base" && ls -A | tr '\n' ' '); }
fail() { echo "FAIL $*"; cat "$T/out"; exit 1; }

sweep killed HEAD~1
[ -f "$T/pid" ] || fail "the fake cargo-mutants never ran"
[ "$status" -ne 0 ] || fail "a killed sweep exited 0"
[ "$(left)" = "cargo-mutants-earlier.tmp " ] \
    || fail "a killed sweep left behind, or removed, files in its base: $(left)"
grep -q 'cargo-mutants-earlier.tmp' "$T/out" || fail "an earlier sweep's copy was not reported"
echo "ok a killed sweep removes its copies and its diff, keeps and reports an earlier copy"

sweep younger HEAD~1
[ "$status" -eq 2 ] || fail "cargo-mutants' status 2 came back as $status"
[ "$(left)" = "cargo-mutants-earlier.tmp cargo-mutants-younger.tmp pstore-mutants-run.younger " ] \
    || fail "a sweep removed a younger sweep's copies, or left its own: $(left)"
echo "ok a sweep keeps a younger sweep's copies, and exits with cargo-mutants' status"

sweep killed HEAD
[ ! -f "$T/pid" ] || fail "a sweep with no diff ran cargo-mutants"
[ "$status" -eq 0 ] || fail "a sweep with no diff exited $status"
[ "$(left)" = "cargo-mutants-earlier.tmp " ] \
    || fail "a sweep that stopped early removed, or left, files in its base: $(left)"
echo "ok a sweep that stops before cargo-mutants removes nothing but its own directory"

# `kill <mutants.sh>` stops the sweep now, not when cargo-mutants finishes.
rm -rf "$T/base" "$T/pid"; mkdir -p "$T/base"
PATH="$T/bin:$PATH" TMPDIR="$T/base" BASE="$T/base" MUTANTS_BASE=HEAD~1 FAKE=hang \
    FAKE_PID="$T/pid" "$R/scripts/mutants.sh" >"$T/out" 2>&1 &
sweeper=$!
for _ in $(seq 100); do [ -s "$T/pid" ] && break; sleep 0.1; done
[ -s "$T/pid" ] || fail "the fake cargo-mutants never started"
kill -TERM "$sweeper"
for _ in $(seq 50); do kill -0 "$sweeper" 2>/dev/null || break; sleep 0.1; done
if kill -0 "$sweeper" 2>/dev/null; then
    kill -9 "$sweeper" "$(cat "$T/pid")" 2>/dev/null || true
    fail "a TERM to mutants.sh did not stop it within 5 s"
fi
status=0; wait "$sweeper" || status=$?
[ "$status" -eq 143 ] || fail "a TERM to mutants.sh exited $status, not 143"
if kill -0 "$(cat "$T/pid")" 2>/dev/null; then
    kill -9 "$(cat "$T/pid")" || true
    fail "a TERM to mutants.sh left cargo-mutants running"
fi
[ -z "$(left)" ] || fail "a stopped sweep left files in its base: $(left)"
echo "ok a TERM stops the sweep at once, cargo-mutants with it, and removes its copies"
