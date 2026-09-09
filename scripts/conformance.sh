#!/usr/bin/env bash
# The conformance suite against the emulators in dev/docker-compose.yml, and the matrix
# D-100 asks for: a dated capability profile per backend, checked into the repo.
#
#   scripts/conformance.sh                probe, and rewrite docs/profiles/capability-matrix.md
#   scripts/conformance.sh --check        probe, and FAIL if the checked-in matrix disagrees
#   scripts/conformance.sh --azure-suffix ask Azurite for a suffix range directly (C-14)
#
# ⚠️ **Outside `cargo test`**, for the reason `recall.sh` and `depth.sh` are: it needs three
# containers, and a suite that cannot run without Docker is a suite that stops running. It is
# also why `gates.sh` does not call it -- and that makes `--check` a gate someone has to run,
# the weakest rung on the gate-design ladder this one can occupy. Named, not hidden.
#
# ⚠️ **Layer 2 tests plumbing** (D-99). `Supported` in the matrix means an emulator answered
# ten probes correctly. It says nothing about S3, GCS or Azure.
set -euo pipefail
cd "$(dirname "$0")/.."

MATRIX=docs/profiles/capability-matrix.md
COMPOSE="docker compose -f dev/docker-compose.yml"
S3=${PSTORE_S3_ENDPOINT:-http://127.0.0.1:9000}
AZ=${PSTORE_AZURE_ENDPOINT:-http://127.0.0.1:10000}
GCS=${PSTORE_GCS_ENDPOINT:-http://localhost:4443}
BUCKET=${PSTORE_S3_BUCKET:-pstore}

CHECK=0
[[ "${1:-}" == "--check" ]] && CHECK=1

# ---------------------------------------------------------------------------
# The containers, and the one bucket each needs. Creating them here rather than
# in the compose file keeps the emulators generic and this script self-contained.
# ---------------------------------------------------------------------------
up() {
    if ! $COMPOSE ps --services --filter status=running 2>/dev/null | grep -q minio; then
        echo "starting the emulator stack" >&2
        $COMPOSE up -d minio azurite gcs >/dev/null
        # No healthcheck on two of the three, so wait for the ports rather than for compose.
        for _ in $(seq 30); do
            curl -sf "$S3/minio/health/live" >/dev/null 2>&1 && break
            sleep 1
        done
    fi
}

make_s3_bucket() {
    $COMPOSE exec -T minio sh -c \
        "mc alias set local http://localhost:9000 ${PSTORE_ACCESS_KEY:-pstore} ${PSTORE_SECRET_KEY:-pstore-dev-secret} >/dev/null && mc mb -p local/$BUCKET >/dev/null" \
        2>/dev/null || true
}

make_gcs_bucket() {
    curl -sf -X POST -H 'Content-Type: application/json' \
        -d "{\"name\":\"${PSTORE_GCS_BUCKET:-pstore}\"}" "$GCS/storage/v1/b" >/dev/null 2>&1 || true
}

# ⚠️ Signed in ONE printf. `$( )` strips trailing newlines, so composing the string-to-sign
# from parts silently drops the newline between the canonicalized headers and the
# canonicalized resource -- and the only symptom is a 403 that reads like a wrong key.
make_azure_container() {
    local acc=devstoreaccount1 ver='2021-08-06'
    local key='Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=='
    local container=${PSTORE_AZURE_CONTAINER:-pstore}
    local date hexkey sig
    date=$(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S GMT')
    hexkey=$(printf '%s' "$key" | base64 -d | xxd -p -c 256)
    sig=$(printf 'PUT\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:%s\nx-ms-version:%s\n/%s/%s/%s\nrestype:container' \
            "$date" "$ver" "$acc" "$acc" "$container" \
          | openssl dgst -sha256 -mac HMAC -macopt "hexkey:$hexkey" -binary | base64 -w0)
    curl -s -o /dev/null -X PUT -H "x-ms-date: $date" -H "x-ms-version: $ver" \
        -H "Authorization: SharedKey $acc:$sig" \
        "$AZ/$acc/$container?restype=container" || true
}

# ⚠️ The one measurement the conformance suite structurally cannot make.
# `object_store` refuses a suffix range client-side, so `suffix_read` for Azurite records the
# CLIENT's answer and would read the same with the emulator stopped. This asks Azurite itself,
# and it is here rather than in a shell history because C-14 rests on it.
#
#   scripts/conformance.sh --azure-suffix   ->  206 for bytes=0-99, 500 for bytes=-1
azure_suffix_probe() {
    local acc=devstoreaccount1 ver='2021-08-06'
    local key='Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=='
    local blob="${PSTORE_AZURE_CONTAINER:-pstore}/conformance/azurite/canary"
    local hexkey; hexkey=$(printf '%s' "$key" | base64 -d | xxd -p -c 256)
    for range in "bytes=0-99" "bytes=-1"; do
        local date sig code
        date=$(LC_ALL=C date -u '+%a, %d %b %Y %H:%M:%S GMT')
        sig=$(printf 'GET\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:%s\nx-ms-range:%s\nx-ms-version:%s\n/%s/%s/%s' \
                "$date" "$range" "$ver" "$acc" "$acc" "$blob" \
              | openssl dgst -sha256 -mac HMAC -macopt "hexkey:$hexkey" -binary | base64 -w0)
        code=$(curl -s -o /dev/null -w '%{http_code}' -H "x-ms-date: $date" -H "x-ms-version: $ver" \
                -H "x-ms-range: $range" -H "Authorization: SharedKey $acc:$sig" \
                "$AZ/$acc/$blob")
        echo "$range -> $code"
    done
}

if [[ "${1:-}" == "--azure-suffix" ]]; then
    up
    make_azure_container
    azure_suffix_probe
    exit 0
fi

up
make_s3_bucket
make_gcs_bucket
make_azure_container

TMP=$(mktemp)
trap 'rm -f "$TMP" "$TMP.check"' EXIT

set +e
PSTORE_S3_ENDPOINT="$S3" PSTORE_AZURE_ENDPOINT="$AZ" PSTORE_GCS_ENDPOINT="$GCS" \
PSTORE_MATRIX_DATE="$(date -u '+%Y-%m-%d')" \
    cargo run --quiet -p pstore-blob --features compat --example probe >"$TMP"
STATUS=$?
set -e

# `--check` must COMPARE, never regenerate: a check that rewrites its own reference passes
# forever and tests nothing.
#
# ⚠️ Two things are normalised away, and only two. The generated date, obviously. And the
# **request duration** `object_store` embeds in its error strings -- "in 1.46ms" against "in
# 1.53ms" is not a capability change, and a check that goes red on scheduler noise is a check
# that gets switched off. Everything else, including the error text itself, is compared
# verbatim: an emulator that starts failing differently is exactly what this exists to catch.
normalize() {
    sed -e 's/^Generated by .* on .*$/Generated: DATE/' \
        -e 's/ in [0-9.]*[munµ]*s / in DURATION /g' "$1"
}

if [[ "$CHECK" == 1 ]]; then
    if [[ ! -f "$MATRIX" ]]; then
        echo "FAIL no $MATRIX to check against" >&2
        exit 1
    fi
    normalize "$TMP" >"$TMP.check"
    normalize "$MATRIX" >"$TMP.matrix"
    if ! diff -u "$TMP.matrix" "$TMP.check"; then
        echo "FAIL the checked-in matrix disagrees with a fresh run" >&2
        rm -f "$TMP.matrix"
        exit 1
    fi
    rm -f "$TMP.matrix"
    echo "ok the matrix matches a fresh run"
else
    # ⚠️ Only when the probe actually finished. A probe that failed to build or died midway
    # leaves $TMP empty, and overwriting the artifact criterion 1 rests on with nothing is a
    # worse outcome than any exit status.
    case "$STATUS" in
        0|4|8|12) mv "$TMP" "$MATRIX"; trap 'rm -f "$TMP.check"' EXIT; echo "wrote $MATRIX" ;;
        *) echo "the probe failed with status $STATUS; $MATRIX left as it was" >&2 ;;
    esac
fi

# ⚠️ **Distinct statuses, and they must reach the caller.** "We could not connect" and "it
# answered wrongly" call for completely different actions, and one non-zero would conflate
# them -- which is how an integration suite ends up reporting green while connected to nothing.
#
# ⚠️ An earlier version ended each arm in an `echo`, so the SCRIPT exited 0 whatever the probe
# said: run with no containers up it rewrote the matrix with three UNREACHABLE sections and
# reported success. Found in code review, and it is the exact failure the paragraph above
# claims this design prevents.
# ⚠️ A BITMASK: 4 = a probed backend diverges, 8 = a backend was unreachable, 12 = both.
# An ordering let an unreachable backend mask a divergence, which is the finding OQ-150 rests
# on; found in code review.
case "$STATUS" in
    0)  echo "all probed backends conform" ;;
    4)  echo "note: a probed backend diverges -- see $MATRIX" >&2 ;;
    8)  echo "note: a backend was unreachable -- see $MATRIX" >&2 ;;
    12) echo "note: a backend was unreachable AND one diverges -- see $MATRIX" >&2 ;;
    *)  echo "the probe itself failed with status $STATUS" >&2 ;;
esac
# `--check` has already decided: it compares against the recorded matrix, so a deployment
# whose recorded state IS divergent stays green. Generate mode reports what it found.
[[ "$CHECK" == 1 ]] && exit 0
exit "$STATUS"
