#!/usr/bin/env bash
# The generated clients, run against a real server (M221).
#
# portable: no -- starts pstore-server on a local port and kills it; CI runs it on ubuntu.
#
# Builds the server, starts it unsecured on the in-memory store with every applied-write
# level on (what `fixture_server` in crates/pstore-server/tests/it/openapi.rs
# builds in process), then runs each client's suite with PSTORE_URL naming it. Every client
# runs api/fixtures/native.json's steps through its generated methods.
#
# ⚠️ Outside `cargo test` for depth.sh's reason, and because it needs Python (and, from M222,
# Node): a mutant of the server rebuilding it once per mutant would buy nothing the in-process
# conformance test does not already check.
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/lib/py.sh

py ./scripts/gen-clients.py --check
cargo build --quiet -p pstore-server --bin pstore-server
BIN=target/debug/pstore-server

LOG=$(mktemp)
PORT=$(py -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
PSTORE_ALLOW_UNAUTHENTICATED=1 PSTORE_LANE=1 PSTORE_BACKEND=memory PSTORE_BIND="127.0.0.1:$PORT" \
    PSTORE_APPLIED_WRITES=5 "$BIN" >"$LOG" 2>&1 &
SERVER=$!
trap 'kill "$SERVER" 2>/dev/null || true; wait "$SERVER" 2>/dev/null || true; rm -f "$LOG"' EXIT

export PSTORE_URL="http://127.0.0.1:$PORT"
up=0
for _ in $(seq 1 100); do
    if py -c 'import sys, urllib.request; urllib.request.urlopen(sys.argv[1] + "/v1/admin/duties", timeout=1)' "$PSTORE_URL" 2>/dev/null; then
        up=1
        break
    fi
    kill -0 "$SERVER" 2>/dev/null || break
    sleep 0.1
done
if [ "$up" != 1 ]; then
    echo "pstore-server did not start:" >&2
    cat "$LOG" >&2
    exit 1
fi

echo "python client"
py -m unittest discover -s clients/python/tests -v
