#!/usr/bin/env bash
# The gossip cost bound per loss pattern, at gate scale (M51), and as the fleet grows (M52).
#
# ⚠️ Outside `cargo test`, for the reason `depth.sh` gives: 50 `Sim` runs of 250 rounds, up to
# 400 members, take minutes, and `cargo mutants` reruns the suite once per mutant. The suite
# keeps one point (200 members, 10% loss, phase 0); this holds a bound per (members, loss) at
# every phase of the drop pattern. M52 adds 800 and 1,600 members, where the leaves double.
#
# ⚠️ Refuses unless both tests passed: a renamed test would otherwise match nothing,
# report "0 passed", and leave this gate green while checking nothing (code review).
set -euo pipefail
cd "$(dirname "$0")/.."
out=$(cargo test --release --quiet -p pstore-gossip --test protocol -- --ignored \
    --exact the_loss_curve_holds_at_every_phase the_cost_scales_with_the_fleet \
    --nocapture "$@" 2>&1) || { printf '%s\n' "$out"; exit 1; }
printf '%s\n' "$out"
case "$out" in
    *"test result: ok. 2 passed"*) ;;
    *) echo "gossip-loss.sh: the loss-curve and fleet-scale tests did not both run" >&2; exit 1 ;;
esac
