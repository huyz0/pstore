#!/usr/bin/env bash
# M57: what a split buys, measured provisionally. One unpeered server against a coordinator of
# three, in one process, over one in-memory store, with no injected wait (`cpu`) and with 20 ms
# a request (`wait`). Every split answer is checked against the unsplit one.
#
# ⚠️ Not a gate, and its numbers are `provisional`: one host's cores are shared by all three
# servers, so this bounds the split's overhead, never its gain on separate machines (M0b).
# Run it with nothing else building.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo test --release -p pstore-server --test split_bench -- --ignored --nocapture
