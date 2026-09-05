#!/usr/bin/env bash
# The recall gate (D-35).
#
# ⚠️ Runs OUTSIDE `cargo test`, and that is the point: a gate-scale corpus inside the unit
# suite is rebuilt once per mutant by `cargo mutants`, which runs the suite hundreds of
# times. Release mode, because brute-force ground truth in a debug build takes minutes.
#
# ⚠️ Every number it prints carries its scale, dimension, dataset and parameters. A recall
# figure without them is not a measurement (evaluation-methodology.md), and these are
# `provisional`: WSL2, synthetic corpus, three orders of magnitude below the 100M the
# roadmap's exit condition names.
#
#   scripts/recall.sh            the gate
#   scripts/recall.sh --sweep    the p x oversample x rerank table behind the chosen defaults
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo run --release --quiet -p pstore-index --example recall -- "$@"
