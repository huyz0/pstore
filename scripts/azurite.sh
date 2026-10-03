#!/usr/bin/env bash
# portable: no -- starts a Node emulator and signs with openssl; gates.sh never runs it.
#
# The Azure backend end to end against Azurite (M46):
#
#   scripts/azurite.sh
#
# Starts Azurite 3.34.0 (installing it with npm into .harness/azurite when absent), creates
# the container, runs pstore-server's ignored test against it, and stops it.
#
# ⚠️ **Outside `cargo test` and `gates.sh`**, as `conformance.sh` is: it needs Node. And not
# through `dev/docker-compose.yml` as `conformance.sh` is, because the environments this repo
# is developed in do not all have a Docker daemon -- M46's did not.
#
# ⚠️ **Emulator evidence** (D-99): provisional, and silent on Azure's latency, cost, and CAS
# under contention, which need a real account (M0b).
set -euo pipefail
cd "$(dirname "$0")/.."

DIR=.harness/azurite
PORT=${PSTORE_AZURITE_PORT:-10000}
ACC=devstoreaccount1
# Azurite's published development key: not a secret.
KEY='Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=='
CONTAINER=${PSTORE_AZURE_CONTAINER:-pstore}
AZ="http://127.0.0.1:$PORT"

if [[ ! -x "$DIR/node_modules/.bin/azurite-blob" ]]; then
    echo "installing azurite 3.34.0 into $DIR" >&2
    mkdir -p "$DIR"
    (cd "$DIR" && npm install --silent --no-audit --no-fund azurite@3.34.0 >/dev/null)
fi

"$DIR/node_modules/.bin/azurite-blob" --blobHost 127.0.0.1 --blobPort "$PORT" \
    --inMemoryPersistence --silent >/dev/null 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true' EXIT
for _ in $(seq 30); do
    curl -s -o /dev/null "$AZ/$ACC?comp=list" && break
    sleep 1
done

# ⚠️ Signed in ONE printf, as `conformance.sh` explains: `$( )` strips trailing newlines.
date=$(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S GMT')
ver='2021-08-06'
hexkey=$(printf '%s' "$KEY" | base64 -d | od -An -v -tx1 | tr -d ' \n')
sig=$(printf 'PUT\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:%s\nx-ms-version:%s\n/%s/%s/%s\nrestype:container' \
        "$date" "$ver" "$ACC" "$ACC" "$CONTAINER" \
      | openssl dgst -sha256 -mac HMAC -macopt "hexkey:$hexkey" -binary | base64 | tr -d '\n')
code=$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "x-ms-date: $date" \
    -H "x-ms-version: $ver" -H "Authorization: SharedKey $ACC:$sig" \
    "$AZ/$ACC/$CONTAINER?restype=container")
# 201 created, 409 already there. Anything else is a container this run cannot use.
if [[ "$code" != 201 && "$code" != 409 ]]; then
    echo "azurite.sh: creating the container answered $code" >&2
    exit 1
fi

PSTORE_LANE=1 PSTORE_BACKEND=azure PSTORE_AZURE_ACCOUNT=$ACC PSTORE_AZURE_KEY=$KEY \
    PSTORE_AZURE_CONTAINER=$CONTAINER PSTORE_AZURE_ENDPOINT="$AZ/$ACC" \
    cargo test -q -p pstore-server --test azure -- --ignored
