#!/usr/bin/env bash
# The mutation gate (D-111), with the fast path as the DEFAULT path.
#
# ⚠️ A full sweep of this workspace is 462 mutants x a 200-second suite -- 25 CPU-hours.
# Nobody runs that in a loop, so anyone who has to type the flags runs nothing instead, and
# the gate that catches "a test which executes code without constraining it" stops running
# exactly where it is most needed: on code that was just written.
#
# So the bare command is incremental. It tests only mutants in the lines this branch
# changed, which for an ordinary commit is tens of mutants and a couple of minutes.
#
#   scripts/mutants.sh                 mutants in this branch's diff against main
#   scripts/mutants.sh --all           every mutant in the workspace (hours)
#   scripts/mutants.sh --file X [Y..]  every mutant in named files
#   scripts/mutants.sh --check REGEX   only mutants whose name matches -- the hypothesis
#                                      mode: seconds, when you already suspect a function
#   scripts/mutants.sh --shard k/n     one shard of a full sweep, for a CI matrix
#
# ⚠️ **`--in-diff` is not a substitute for a full sweep**, and the failure is specific:
# cargo-mutants matches the diff against the code UNDER TEST, so a commit that only changes
# *test* code produces no mutants and passes in seconds while having materially changed the
# test suite's strength. The nightly `--all` run is what covers that.
#
# ⚠️ **Never run another `cargo` command while a sweep is going.** Timeouts are wall-clock,
# and contention turns them into false results: the M5 sweep reported 1 timeout while it had
# the machine and 17 once a `cargo test` was running beside it. Every one of the 17 was a
# mutant that cannot hang -- `fuse -> vec![]`, `query -> Ok(vec![])`.
set -euo pipefail
cd "$(dirname "$0")/.."

# ⚠️ A ramdisk if one has room, and the check is not decoration: cargo-mutants copies the
# tree per job, and eight copies of `target/` filled a 23 GB tmpfs mid-run and died with
# ENOSPC. `debug = "none"` (see `.cargo/mutants.toml`) is what makes them fit at all.
pick_tmpdir() {
    local need free
    need=$(( $(du -sm target 2>/dev/null | cut -f1 || echo 4096) ))
    free=$(df -Pm /tmp | awk 'NR==2 {print $4}')
    if [[ -n "$free" && "$free" -gt $(( need * (JOBS + 1) )) ]]; then
        mkdir -p /tmp/pstore-mutants && echo /tmp/pstore-mutants
    else
        mkdir -p "${HOME}/.cache/pstore-mutants" && echo "${HOME}/.cache/pstore-mutants"
    fi
}

# A faster linker if this machine has one. `mutants.rs` measures mold at ~20% and wild at
# better than half; both are multiplicative with everything else here, and neither is
# required -- this is why the choice is detected rather than configured.
linker_flags() {
    if command -v mold >/dev/null 2>&1; then
        echo "-C link-arg=-fuse-ld=mold"
    elif command -v wild >/dev/null 2>&1; then
        echo "-C link-arg=--ld-path=wild"
    fi
}

JOBS=${MUTANTS_JOBS:-$(( $(nproc) / 4 ))}
[[ "$JOBS" -lt 1 ]] && JOBS=1

args=()
case "${1:-}" in
    --all)   shift ;;
    --file)  shift; for f in "$@"; do args+=(--file "$f"); done; set -- ;;
    --check) shift; args+=(-F "$1"); shift ;;
    --shard) shift; args+=(--shard "$1"); shift ;;
    *)
        # The merge base, so a stale main does not widen the diff to everything.
        base=$(git merge-base HEAD "${MUTANTS_BASE:-origin/main}" 2>/dev/null \
            || git merge-base HEAD main 2>/dev/null || echo HEAD~1)
        diff=$(mktemp)
        trap 'rm -f "$diff"' EXIT
        git diff "$base"...HEAD > "$diff"
        if [[ ! -s "$diff" ]]; then
            echo "no diff against $base -- nothing to mutate. Use --all for a full sweep." >&2
            exit 0
        fi
        args+=(--in-diff "$diff")
        ;;
esac

export TMPDIR="${TMPDIR:-$(pick_tmpdir)}"
flags=$(linker_flags)
[[ -n "$flags" ]] && export RUSTFLAGS="${RUSTFLAGS:-} $flags"

echo "# jobs=$JOBS tmpdir=$TMPDIR linker=${flags:-default}" >&2
exec cargo mutants -j "$JOBS" --no-shuffle "${args[@]}" "$@"
