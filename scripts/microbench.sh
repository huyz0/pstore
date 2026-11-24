#!/usr/bin/env bash
# M276: per-feature microbenchmarks -- writes (array and base64 f32, columns, text-only) and
# queries (BM25, filters, sparse, MaxSim, ANN, RRF, computed distance, highlights) -- over one
# in-memory store with no injected wait. Each case prints its time's median, p10, p90 and MAD,
# its allocations and bytes per operation, and a digest of its answers, and the CSV goes to
# `$MICROBENCH_CSV` (default target/microbench.csv).
#
# ⚠️ Not a gate, and its numbers are `provisional` (AGENTS.md): relative, on this host only.
# Run it with nothing else building, and twice: the digests must match, and the medians'
# spread is the noise.
set -euo pipefail
cd "$(dirname "$0")/.."
export MICROBENCH_CSV="${MICROBENCH_CSV:-$PWD/target/microbench.csv}"
export MICROBENCH_COMMIT="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
export MICROBENCH_RUSTC="$(rustc --version)"
exec cargo test --release -p pstore-server --test microbench -- --ignored --nocapture
