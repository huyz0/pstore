#!/usr/bin/env bash
# The mutation gate (D-111), with the fast path as the DEFAULT path.
#
# ⚠️ A full sweep of this workspace is **3,339 mutants** (`cargo mutants --list` through the
# entry-point excludes, 2026-09-23; 3,401 before M8c and M8d, 462 in M5), and one CI shard of
# ~420 takes about two hours -- roughly two minutes of workspace suite per viable mutant.
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
# ⚠️ **A timeout is not a kill, and cargo-mutants' auto-timeout is too tight here.** It sets
# the limit from the baseline, and this suite's baseline is ~20 s -- so the limit came out at
# 20 s and every mutant that SURVIVED, running the suite to completion, was cut off and
# reported as a timeout instead. Measured: 18 "timeouts" in `sparse.rs`, 14 of them the
# `| -> ^` pairs that are provably equivalent and therefore cannot fail a test. A gate that
# reports a result it did not measure is worse than one that reports nothing, so the floor is
# set explicitly below.
#
# ⚠️ **Never run another `cargo` command while a sweep is going.** With headroom this small,
# contention alone can push a passing run past the limit.
# ⚠️ **This project's rule is that local builds and tests run in the container**
# (`dev/README.md`: "WSL2 host, everything containerized and resource-capped"), where the
# compose file caps `dev` at 6 CPUs and 8 GB so a runaway OOMs one container rather than the
# VM. A sweep is the heaviest thing here and the least excusable place to skip it:
#
#   docker compose -f dev/docker-compose.yml exec dev scripts/mutants.sh
#
# The caps below are the fallback for when it is run on the host anyway. They are a fallback,
# not a substitute -- the container is the only one of the three ceilings this script controls.
# portable: no -- sized from cgroup memory and `nproc`; it runs in the Linux dev container
# and on CI's ubuntu runner, never on a developer's bare host.
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/lib/py.sh

# ⚠️ **Build directories go on DISK, never tmpfs, and this is a WSL2 rule with teeth.**
# On WSL2 `/tmp` is a tmpfs, which means it is RAM. An earlier version of this script picked
# it whenever `df /tmp` showed room -- but that "free space" is free *memory*, and filling it
# with 1.5 GB build directories while N `rustc` processes are also asking for memory is how
# the VM dies. **It killed this machine twice.** Opt in with `MUTANTS_TMPDIR` on a host where
# /tmp is real storage; the default is the disk-backed cache.
pick_tmpdir() {
    if [[ -n "${MUTANTS_TMPDIR:-}" ]]; then
        mkdir -p "$MUTANTS_TMPDIR" && echo "$MUTANTS_TMPDIR"
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

# ⚠️ **Jobs are capped by MEMORY, not by cores**, because memory is what fails. Each job is a
# `cargo` that builds and links, and linking is the peak -- `dev/cargo-config.sample.toml` says
# so in its first line. Budget ~4 GB a job and never exceed half the cores.
#
# ⚠️ And each job's cargo is itself capped, because without `.cargo/config.toml` a `cargo build`
# fans out to every core: 8 jobs x 20 internal build jobs is 160 concurrent rustc invocations
# on a machine with no `.wslconfig` ceiling. That is what took this VM down, twice.
# ⚠️ The CGROUP limit first, because `/proc/meminfo` inside a container reports the HOST's
# memory -- so a container capped at 8 GB would size its jobs against the host's 44 and defeat
# the cap it was put there for.
if [[ -r /sys/fs/cgroup/memory.max ]] && [[ "$(cat /sys/fs/cgroup/memory.max)" != "max" ]]; then
    mem_gb=$(( $(cat /sys/fs/cgroup/memory.max) / 1073741824 ))
else
    mem_gb=$(awk '/MemAvailable/ {print int($2/1048576)}' /proc/meminfo 2>/dev/null || echo 8)
fi
by_mem=$(( mem_gb / 4 ))
by_cpu=$(( $(nproc) / 2 ))
JOBS=${MUTANTS_JOBS:-$(( by_mem < by_cpu ? by_mem : by_cpu ))}
[[ "$JOBS" -lt 1 ]] && JOBS=1
[[ "$JOBS" -gt 8 ]] && JOBS=8
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

args=()
scope=()
case "${1:-}" in
    --all)   shift ;;
    # Both spellings: `--file a b c` and `--file a --file b --file c`. The second is what
    # `cargo mutants` itself takes, so typing it here is the natural mistake -- and the loop
    # used to turn the literal `--file` tokens into paths, which cargo-mutants reports as
    # "a value is required for '--file'" rather than as anything a reader can act on.
    --file)
        shift
        files=()
        for f in "$@"; do [[ "$f" == "--file" ]] || { args+=(--file "$f"); files+=("$f"); }; done
        set --
        # ⚠️ Only the packages whose tests could possibly catch a mutant in those files --
        # the reverse-dependency closure, dev-dependencies included. `test_workspace = true`
        # exists because a decorator's tests can live in another crate; "every package" is
        # not the fix for that, "every package that could see it" is. Nothing in
        # `pstore-gossip` can catch a mutant in `pstore-format`, and its tests cost 7.5 s on
        # every one of them.
        while read -r pkg; do
            [[ -n "$pkg" ]] && scope+=(--test-package "$pkg")
        done < <(./scripts/mutants-scope.py "${files[@]}" 2>/dev/null || true)
        [[ ${#scope[@]} -gt 0 ]] && args+=(--test-workspace=false "${scope[@]}")
        ;;
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

# ⚠️ **Binary entry points are never mutated** (M8d): no test runs a binary, so no test can
# catch a mutant in one, and a sweep that mutates them can never go green. Derived by
# `scripts/lib/scope.py`, the same derivation `coverage.sh` uses -- but ENTRY POINTS ONLY:
# `ships = false` crates stay mutated, because they are testable and hold the conformance
# probes. Captured and checked, never read through `< <(...)`: a silent failure here would put
# every excluded mutant back and turn the nightly red with no explanation.
if ! metadata=$(cargo metadata --no-deps --format-version 1) \
    || ! excludes=$(printf '%s' "$metadata" | py scripts/lib/scope.py mutants); then
    echo "FAIL could not derive the mutation scope (scripts/lib/scope.py)" >&2
    exit 1
fi
while IFS= read -r glob; do
    [[ -n "$glob" ]] && args+=(--exclude "$glob")
done <<< "$excludes"
echo "# not mutated (entry points): $(printf '%s' "$excludes" | tr '\n' ' ')" >&2

export TMPDIR="${TMPDIR:-$(pick_tmpdir)}"
flags=$(linker_flags)
[[ -n "$flags" ]] && export RUSTFLAGS="${RUSTFLAGS:-} $flags"

echo "# jobs=$JOBS cargo-build-jobs=$CARGO_BUILD_JOBS tmpdir=$TMPDIR linker=${flags:-default}" >&2
# ⚠️ Headroom over the baseline, not a multiple of it. A genuinely hung mutant -- `put_varint`
# with its loop condition flipped is one -- takes the full 300 s and is still caught; a
# survivor finishes in the baseline's 20 s and is reported honestly as MISSED.
exec cargo mutants -j "$JOBS" --minimum-test-timeout 300 --no-shuffle "${args[@]}" "$@"
