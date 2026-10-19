#!/usr/bin/env bash
# portable: no -- needs a Docker daemon; gates.sh never runs it.
#
# GCS's batch delete end to end against fake-gcs-server (M107):
#
#   scripts/fake-gcs.sh
#
# Starts fake-gcs-server 1.55 in Docker, creates the bucket, runs pstore-blob's ignored GCS
# test against it, prints what the emulator itself logged, and stops it.
#
# ⚠️ **Outside `cargo test` and `gates.sh`**, as `azurite.sh` is: it needs Docker, which not
# every environment this repo is developed in has.
#
# ⚠️ **Emulator evidence** (D-99): provisional, and silent on real GCS's throttling of a batch
# (M0b). The image comes from `mirror.gcr.io` because Docker Hub rate-limits anonymous pulls.
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=${PSTORE_FAKE_GCS_IMAGE:-mirror.gcr.io/fsouza/fake-gcs-server:1.55}
PORT=${PSTORE_GCS_PORT:-4443}
BUCKET=${PSTORE_GCS_BUCKET:-pstore}
NAME=pstore-fake-gcs-$$
GCS="http://localhost:$PORT"

docker run -d --rm --name "$NAME" -p "$PORT:4443" "$IMAGE" \
    -scheme http -public-host "localhost:$PORT" >/dev/null
trap 'docker stop "$NAME" >/dev/null 2>&1 || true' EXIT
for _ in $(seq 30); do
    curl -s -o /dev/null "$GCS/storage/v1/b" && break
    sleep 1
done

code=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    -d "{\"name\":\"$BUCKET\"}" "$GCS/storage/v1/b")
# 200 created, 409 already there. Anything else is a bucket this run cannot use.
if [[ "$code" != 200 && "$code" != 409 ]]; then
    echo "fake-gcs.sh: creating the bucket answered $code" >&2
    exit 1
fi

PSTORE_GCS_ENDPOINT=$GCS PSTORE_GCS_BUCKET=$BUCKET \
    cargo test -q -p pstore-blob --features gcs --test gcs -- --ignored

# The emulator's own count, beside the client's: a top-level request carries the client's
# address, and a batch's sub-requests are logged without one.
echo "fake-gcs.sh: top-level requests the emulator logged:"
docker logs "$NAME" 2>&1 | grep -oE '[0-9.]+ - - \[[^]]*\] \\"(POST|DELETE|GET|PUT) [^ ?]*' \
    | sed -E 's/.*"(POST|DELETE|GET|PUT) /\1 /; s|/o/.*|/o/...|; s|^(GET\|PUT\|DELETE) /[^/]*/.*|\1 /{bucket}/...|' \
    | sort | uniq -c
