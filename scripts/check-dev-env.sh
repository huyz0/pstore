#!/usr/bin/env bash
# The local build ceilings exist, on a host where their absence takes the machine down.
#
# ⚠️ Rung 3, and it is here because rung 7 failed. `dev/README.md` step 2 has said "copy
# `cargo-config.sample.toml` to `.cargo/config.toml`" since the environment was designed, and
# on this machine it had never been done — so every `cargo build` fanned out to all 20 cores
# with full debug info. A mutation sweep on top of that killed the WSL2 VM **twice**.
#
# `gate-design`: if the rule can be stated as a predicate over files in the tree, nobody
# should be asked to remember it. This is that predicate.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
note() { echo "  $*" >&2; }

if [[ ! -f .cargo/config.toml ]]; then
    echo "MISSING .cargo/config.toml -- the build-memory cap" >&2
    note "cp dev/cargo-config.sample.toml .cargo/config.toml"
    note "(drop the [target.*] linker section unless clang and mold are installed)"
    fail=1
elif ! grep -qE '^\s*jobs\s*=' .cargo/config.toml; then
    echo "MISSING [build] jobs in .cargo/config.toml -- cargo will use every core" >&2
    note "linking is the peak; see dev/cargo-config.sample.toml"
    fail=1
fi

# ⚠️ WSL2 only, and a warning rather than a failure: `.wslconfig` lives on the Windows side,
# so CI has no such file and neither does a Linux host. What it catches is the ceiling that
# protects the machine the VM runs on -- the one whose absence is fatal rather than slow.
if grep -qi microsoft /proc/version 2>/dev/null; then
    cfg=$(ls /mnt/c/Users/*/.wslconfig 2>/dev/null | head -1 || true)
    total=$(awk '/MemTotal/ {printf "%d", $2/1048576}' /proc/meminfo)
    if [[ -z "$cfg" ]]; then
        echo "WARN no .wslconfig: the VM may take up to half the host's RAM" >&2
        note "see dev/wslconfig.sample, then: wsl --shutdown"
    elif ! grep -qi '^processors' "$cfg"; then
        echo "WARN $cfg sets no processor cap; the VM sees $(nproc) cores and ${total}GB" >&2
        note "see dev/wslconfig.sample, then: wsl --shutdown"
    fi
fi

if [[ "$fail" -ne 0 ]]; then
    echo "FAIL the local build ceilings are not in place" >&2
    exit 1
fi
echo "ok build ceilings in place"
