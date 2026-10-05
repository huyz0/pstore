#!/usr/bin/env bash
# The gossip cost bound per loss pattern, at gate scale (M51).
#
# ⚠️ Outside `cargo test`, for the reason `depth.sh` gives: 50 `Sim` runs of 250 rounds, up to
# 400 members, take minutes, and `cargo mutants` reruns the suite once per mutant. The suite
# keeps one point (200 members, 10% loss, phase 0); this holds a bound per (members, loss) at
# every phase of the drop pattern.
#
# ⚠️ Refuses unless exactly one test passed: a renamed test would otherwise match nothing,
# report "0 passed", and leave this gate green while checking nothing (code review).
set -euo pipefail
cd "$(dirname "$0")/.."
out=$(cargo test --release --quiet -p pstore-gossip --test protocol -- --ignored \
    --exact the_loss_curve_holds_at_every_phase --nocapture "$@" 2>&1) || { printf '%s\n' "$out"; exit 1; }
printf '%s\n' "$out"
case "$out" in
    *"test result: ok. 1 passed"*) ;;
    *) echo "gossip-loss.sh: the loss-curve test did not run" >&2; exit 1 ;;
esac
