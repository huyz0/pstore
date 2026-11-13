#!/usr/bin/env bash
# The scalar-storage benchmark (M230 onward): blocks, GETs, bytes and mask time per filter.
#
# ⚠️ A measurement, not a gate: it asserts only that every mask admits exactly the rows a
# reference admits. Times are `provisional` (in-memory store, one machine) and relative
# between arms of one run. Outside `cargo test` for `depth.sh`'s reason.
#
#   scripts/columns.sh            20,000 rows (gate scale)
#   scripts/columns.sh 100000     another scale
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo run --release --quiet -p pstore-query --example columns -- "$@"
