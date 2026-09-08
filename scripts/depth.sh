#!/usr/bin/env bash
# The round-trip and byte gates at GATE SCALE (M5a c9/c11, M5c c6/c7).
#
# ⚠️ Outside `cargo test`, for the reason `recall.sh` gives: `cargo mutants` reruns the suite
# once per mutant -- 462 times in M5 -- so a 36-second fixture inside the suite costs four and
# a half hours across a sweep. The suite keeps small-scale versions that pin the shape; the
# ones whose whole point is scale live here.
#
# ⚠️ Scale is not decoration. M1's depth invariant held at 500 rows and broke at 40,000.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo run --release --quiet -p pstore-index --example depth -- "$@"
