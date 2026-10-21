#!/usr/bin/env bash
# M119: where one server spends its CPU -- ingest, fold, and each query kind -- over one
# in-memory store with no injected wait. Every kind prints a digest of its answers, so a
# change that is faster because it answers differently is visible.
#
# ⚠️ Not a gate, and its numbers are `provisional` (AGENTS.md): relative, on this host only.
# Run it with nothing else building.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo test --release -p pstore-server --test query_bench -- --ignored --nocapture
