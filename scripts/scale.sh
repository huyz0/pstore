#!/usr/bin/env bash
# M6's exit criterion: 1M indexes, open cost against index count, zero LISTs on any hot path.
#
# ⚠️ **Not a gate, and not in `gates.sh`.** The catalog arm peaks near 9.3 GB and the two
# harnesses together take about a minute — the same argument that keeps `recall.sh` and
# `depth.sh` out of `cargo test`. A CI runner that must hold 9 GB is a different problem from
# measuring this once.
#
# ⚠️ Nothing is downloaded and nothing is vendored: the workload is synthetic, so the only
# input is the tenant count.
#
#   scripts/scale.sh                  both harnesses, 1M tenants
#   PSTORE_SCALE_SMALL=1 PSTORE_OPEN_SMALL=1 scripts/scale.sh   the small arms, for checking
#                                     the harnesses without the memory
set -euo pipefail
cd "$(dirname "$0")/.."

# ⚠️ `--release`. A debug build seeds a million tenants at a rate that makes the run look
# broken rather than slow, and the memory is worse.
cargo run --release -q -p pstore-catalog --example scale
echo
cargo run --release -q -p pstore-engine --example open
