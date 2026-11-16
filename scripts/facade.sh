#!/usr/bin/env bash
# The vendor's own turbopuffer Python SDK against pstore's compatibility facade (M268).
#
# portable: no -- starts pstore-server on a local port, builds a virtual environment and
# downloads a wheel; CI runs it on ubuntu.
#
# Starts the server unsecured on the in-memory store (as clients.sh does), downloads the
# pinned SDK wheel, refuses it unless its sha256 is the pinned one, installs it into a scratch
# virtual environment, and runs clients/turbopuffer's suite with PSTORE_URL naming the server.
# The SDK is MIT and honours a base URL, so no vendor account is needed. Its dependencies are
# resolved by pip within the SDK's own bounds; the SDK itself is pinned by hash.
#
# ⚠️ The SDK needs Python 3.11 or later; the interpreter is `$FACADE_PYTHON`, else `py`.
# ⚠️ Outside `cargo test` for depth.sh's reason, and because it needs the network for pip.
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/lib/py.sh

SDK=turbopuffer==3.1.0
SDK_SHA256=f5e44b540902f58e8201a62d0df0e98e8fe04987c41efec4bf8a83ea35254389

cargo build --quiet -p pstore-server --bin pstore-server
BIN=target/debug/pstore-server

WORK=$(mktemp -d)
LOG="$WORK/server.log"
SERVER=""
cleanup() {
    if [ -n "$SERVER" ]; then
        kill "$SERVER" 2>/dev/null || true
        wait "$SERVER" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

if [ -n "${FACADE_PYTHON:-}" ]; then
    "$FACADE_PYTHON" -m venv "$WORK/venv"
else
    py -m venv "$WORK/venv"
fi
VPY="$WORK/venv/bin/python"
"$VPY" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else "the turbopuffer SDK needs Python 3.11 or later")'
"$VPY" -m pip download --quiet --no-deps --only-binary=:all: --dest "$WORK/wheel" "$SDK"
WHEEL=$(ls "$WORK"/wheel/turbopuffer-*.whl)
GOT=$("$VPY" -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$WHEEL")
if [ "$GOT" != "$SDK_SHA256" ]; then
    echo "the downloaded $SDK wheel is not the pinned one: sha256 $GOT, want $SDK_SHA256" >&2
    exit 1
fi
"$VPY" -m pip install --quiet "$WHEEL"

PORT=$(py -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
PSTORE_ALLOW_UNAUTHENTICATED=1 PSTORE_LANE=1 PSTORE_BACKEND=memory PSTORE_BIND="127.0.0.1:$PORT" \
    PSTORE_APPLIED_WRITES=5 "$BIN" >"$LOG" 2>&1 &
SERVER=$!

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

echo "turbopuffer SDK against the facade"
"$VPY" -m unittest discover -s clients/turbopuffer -p 'test_*.py' -v
