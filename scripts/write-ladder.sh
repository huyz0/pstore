#!/usr/bin/env bash
# An open-loop write-throughput ladder (M284): fixed arrival rates, latency counted from when
# each write was due, stopping at the first step under 95% acknowledged or over 5 s p99.
# Prints docs/s a core at the knee, for one tenant, 64 tenants, and 64 tenants in bulks of 10.
#
# ⚠️ Outside `cargo test`, for the reason `depth.sh` gives. The suite keeps a low step of
# every shape (`write_ladder::a_low_rate_holds_on_every_shape`).
#
# ⚠️ Refuses unless the test passed: a renamed test would otherwise match nothing, report
# "0 passed", and leave this green while measuring nothing (gossip-loss.sh's lesson).
set -euo pipefail
cd "$(dirname "$0")/.."
out=$(cargo test --release --quiet -p pstore-server --test it -- --ignored \
    --exact write_ladder::the_write_ladder --nocapture "$@" 2>&1) \
    || { printf '%s\n' "$out"; exit 1; }
printf '%s\n' "$out"
case "$out" in
    *"test result: ok. 1 passed"*) ;;
    *) echo "write-ladder.sh: the ladder did not run" >&2; exit 1 ;;
esac
