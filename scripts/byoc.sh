#!/usr/bin/env bash
# M7g — the BYOC smoke test: two server containers, one bucket, one write across a process
# boundary.
#
#   scripts/byoc.sh            build the image and run every arm
#   scripts/byoc.sh --keep     leave the containers up for inspection
#
# ⚠️ **This is the only test in the repository that crosses a process boundary.** Every other
# durability proof is two `Api`s in one test binary, which is honest about the memtable and
# says nothing about two machines sharing a bucket.
#
# ⚠️ Outside `cargo test` because it needs Docker, which puts it on `gate-design`'s weakest
# rung: a gate someone must run. It is listed in `dev/README.md` beside `conformance.sh`, and
# NOT in AGENTS.md's Gates table -- `scripts/build-index.py --check` requires that table and
# CI to be the same set in both directions, and CI has no Docker.
#
# ⚠️ Host networking and its own RustFS, following `scripts/cluster.sh`: a plain `docker run`
# is not on the compose project's network, so `http://rustfs:9000` would not resolve -- and
# the bucket is made here, with a SigV4-signed `curl`, because nothing else makes it.
# portable: no -- host networking between containers, and `timeout` is the hang guard that
# turns a regressed door guard into a failure instead of a gate that never returns.
set -euo pipefail
cd "$(dirname "$0")/.."
# ⚠️ Git Bash rewrites an argument that looks like a POSIX path into a Windows one, so the
# container argument `/data` reached RustFS as `C:/Program Files/Git/data` and it exited
# "Volume not found" -- a store that never came up, reported as a connection refused. A no-op
# anywhere but Git Bash.
export MSYS_NO_PATHCONV=1

IMAGE=pstore-byoc
STORE=pstore-byoc-rustfs
STORE_PORT=9210
A_PORT=8211
B_PORT=8212
BUCKET=pstore
TENANT=42
KEEP=${1:-}
fails=0

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
ok()   { printf '   ok   %s\n' "$*"; }
bad()  { printf '   FAIL %s\n' "$*"; fails=$((fails + 1)); }

cleanup() {
  [ "$KEEP" = "--keep" ] && { echo "left up: $STORE, ${IMAGE}-a, ${IMAGE}-b"; return; }
  docker rm -f "$STORE" "${IMAGE}-a" "${IMAGE}-b" >/dev/null 2>&1 || true
}
trap cleanup EXIT

CURL="curlimages/curl:8.11.1"

# ⚠️ **Every request is made FROM A CONTAINER on the host network, never from this shell.**
# Two reasons, and the second is the load-bearing one: the host may not have curl, and — as
# measured here — a shell may sit in a namespace that cannot reach a host-network container's
# port, in which case a gate run from the host hangs on connect while the server is perfectly
# healthy. A container asking a container is the same path a real client takes.
#
# ⚠️ `--max-time`, always. A curl with no deadline turns "the server never answered" into a
# gate that hangs instead of failing, which is the one outcome a gate must not have.
#
# Prints the body, then a final line holding the status. ONE request, so the cost counters
# `/metrics` reports are the ones the test actually caused.
req() {
  docker run --rm --network host "$CURL" -s --max-time 30 -w '\n%{http_code}' "$@" 2>/dev/null
}
body() { sed '$d'; }
code() { tail -1; }

# Runs a server container. $1 = name suffix, $2 = port, $3 = lane, rest = extra -e flags.
server() {
  local name=$1 port=$2 lane=$3; shift 3
  docker rm -f "${IMAGE}-${name}" >/dev/null 2>&1 || true
  # ⚠️ `0.0.0.0`, not `127.0.0.1:$port`. A first draft bound the loopback and criterion 5
  # caught it immediately -- which is the mutation that criterion names, made by the harness
  # rather than by the image.
  docker run -d --name "${IMAGE}-${name}" --network host \
    -e PSTORE_BIND="0.0.0.0:${port}" \
    -e PSTORE_LANE="$lane" \
    -e PSTORE_BACKEND=s3 \
    -e PSTORE_S3_ENDPOINT="http://127.0.0.1:${STORE_PORT}" \
    -e PSTORE_BUCKET="$BUCKET" \
    -e PSTORE_ACCESS_KEY=pstore \
    -e PSTORE_SECRET_KEY=pstore-dev-secret \
    "$@" "$IMAGE" >/dev/null
}

wait_up() { # $1 = port
  for _ in $(seq 1 40); do
    [ "$(req "http://127.0.0.1:$1/metrics" | code)" = "200" ] && return 0
    sleep 0.25
  done
  return 1
}

say "building $IMAGE"
docker build -f dev/Dockerfile.server -t "$IMAGE" . >/dev/null

say "RustFS on :$STORE_PORT"
docker rm -f "$STORE" >/dev/null 2>&1 || true
# ⚠️ The console off: it is a second port per instance, and under host networking a second
# port is a second collision.
docker run -d --name "$STORE" --network host \
  -e RUSTFS_ACCESS_KEY=pstore -e RUSTFS_SECRET_KEY=pstore-dev-secret \
  -e RUSTFS_ADDRESS="0.0.0.0:$STORE_PORT" -e RUSTFS_CONSOLE_ENABLE=false \
  --memory 1g --cpus 2 \
  rustfs/rustfs:1.0.0 /data >/dev/null
for _ in $(seq 1 40); do
  [ "$(req "http://127.0.0.1:$STORE_PORT/health" | code)" = "200" ] && break
  sleep 0.25
done
# ⚠️ The status is ASSERTED. The `mc mb ... 2>&1` this replaces discarded its outcome, so a
# bucket that was never made surfaced three arms later as a server that could not write.
r=$(req --aws-sigv4 "aws:amz:us-east-1:s3" --user pstore:pstore-dev-secret -X PUT \
  -H "x-amz-content-sha256: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855" \
  "http://127.0.0.1:$STORE_PORT/$BUCKET")
[ "$(code <<<"$r")" = "200" ] || { bad "could not create bucket $BUCKET: $r"; exit 1; }

# --- Criterion 3: an unprobed profile refuses, and the container EXITS. -----------------
# ⚠️ `PSTORE_BACKEND=s3` matters: with the default `memory` the profile is Supported and the
# container serves correctly, so an arm that omitted it would pass while proving nothing.
say "criterion 3 — an unprobed profile refuses to serve"
# ⚠️ `timeout`, and 124 is a FAILURE. Without it the arm hangs on the exact mutation it
# names: a door guard that regressed means the container SERVES, the command substitution
# never returns, and the `bad` line below is unreachable. The script's own header forbids that
# for `req`; a foreground `docker run` is the same hazard wearing different clothes.
out=$(timeout 30 docker run --rm --network host \
  -e PSTORE_LANE=1 -e PSTORE_BACKEND=s3 \
  -e PSTORE_S3_ENDPOINT="http://127.0.0.1:${STORE_PORT}" "$IMAGE" 2>&1) && code=0 || code=$?
case "$code" in
  0)   bad "served with an unprobed profile" ;;
  124) bad "did not exit -- it is still serving with an unprobed profile" ;;
  *)   ok "exited $code" ;;
esac
case "$out" in
  *"refusing to serve"*"s3(http://127.0.0.1:${STORE_PORT})"*) ok "named the backend" ;;
  *) bad "did not name the backend: $out" ;;
esac
# ⚠️ `cas=Divergent`, not `*cas*`: the loose form passes on any message that happens to
# contain the substring, and asserts nothing about the primitive that actually diverged.
case "$out" in *"cas=Divergent"*) ok "named the primitive" ;; *) bad "did not name the primitive: $out" ;; esac

# --- Criterion 4: a lane is required in the image too. ----------------------------------
say "criterion 4 — no lane, no server"
out=$(timeout 30 docker run --rm --network host -e PSTORE_BACKEND=memory "$IMAGE" 2>&1) && code=0 || code=$?
case "$code" in
  0)   bad "started without a lane" ;;
  124) bad "did not exit -- the image has a default lane" ;;
  *)   ok "exited $code" ;;
esac
case "$out" in *PSTORE_LANE*) ok "named the variable" ;; *) bad "did not say why: $out" ;; esac

# --- Criteria 1, 2: two containers, one bucket. -----------------------------------------
say "criteria 1,2 — two containers, one bucket"
server a "$A_PORT" 1 -e PSTORE_PROFILE=conforming
server b "$B_PORT" 2 -e PSTORE_PROFILE=conforming
wait_up "$A_PORT" || { bad "container A never came up"; docker logs "${IMAGE}-a"; exit 1; }
wait_up "$B_PORT" || { bad "container B never came up"; docker logs "${IMAGE}-b"; exit 1; }
ok "both up, lanes 1 and 2"

A="http://127.0.0.1:$A_PORT"
B="http://127.0.0.1:$B_PORT"
H=(-H "X-Pstore-Tenant: $TENANT" -H 'Content-Type: application/json')

r=$(req -X PUT "${H[@]}" \
  -d '{"durability":"durable","documents":[{"id":"doc-1","vector":[1,0,0,0],"text":"across the boundary"}]}' \
  "$A/v1/indexes/byoc/documents")
if [ "$(code <<<"$r")" = "200" ] && grep -q '"durable":true' <<<"$r"; then
  ok "A wrote durably"
else
  bad "A refused the write, or called it not durable: $r"
fi

# ⚠️ **After A's write and BEFORE A's fold**, and the order is the whole assertion. A first
# draft asked this before the write, where container A would have answered 404 as well —
# vacuous, and green. Asked here it is a harness check, not a system mutation: `Engine::query`
# re-reads HEAD every time, so B provably cannot see A's unfolded bundle. What it catches is
# both requests aimed at A, whose memtable answers 200 with the document and makes criterion 2
# prove nothing at all.
r=$(req -X POST "${H[@]}" -d '{"vector":[1,0,0,0],"top_k":1}' "$B/v1/indexes/byoc/query")
[ "$(code <<<"$r")" = "404" ] && ok "B cannot see A's unfolded write" || bad "B answered before any fold: $r"

r=$(req -X POST -H "X-Pstore-Tenant: $TENANT" "$A/v1/admin/fold")
[ "$(code <<<"$r")" = "200" ] && ok "A folded" || bad "A's fold failed: $r"

r=$(req -X POST "${H[@]}" -d '{"vector":[1,0,0,0],"top_k":1}' "$B/v1/indexes/byoc/query")
if [ "$(code <<<"$r")" = "200" ] && grep -q 'doc-1' <<<"$r"; then
  ok "B — a different process, an empty memtable — returned the document"
else
  bad "B did not return the document: $r"
fi

# ⚠️ **And back the other way, which is not symmetry for its own sake.** Until B writes,
# `lanes::register` is CASed exactly once by one process and lane 2 never appears in the
# registry -- so the second of the two CAS'd objects in this system, the one with no ABA nonce
# behind it, would go completely untested by the milestone that exists to test two writers
# against one bucket.
r=$(req -X PUT "${H[@]}" \
  -d '{"durability":"durable","documents":[{"id":"doc-2","vector":[0,1,0,0],"text":"the other lane"}]}' \
  "$B/v1/indexes/byoc/documents")
[ "$(code <<<"$r")" = "200" ] && ok "B wrote durably on lane 2" || bad "B refused the write: $r"
r=$(req -X POST -H "X-Pstore-Tenant: $TENANT" "$B/v1/admin/fold")
[ "$(code <<<"$r")" = "200" ] && ok "B folded" || bad "B's fold failed: $r"

r=$(req -X POST "${H[@]}" -d '{"vector":[0,1,0,0],"top_k":2}' "$A/v1/indexes/byoc/query")
if [ "$(code <<<"$r")" = "200" ] && grep -q 'doc-2' <<<"$r" && grep -q 'doc-1' <<<"$r"; then
  ok "A sees both lanes — two writers, one HEAD, nothing lost"
else
  bad "A lost a lane: $r"
fi

# --- Criterion 5: /metrics from outside the container. ----------------------------------
say "criterion 5 — /metrics is reachable from outside"
m=$(req "$B/metrics" | body)
case "$m" in
  *'pstore_http_requests_total{route="/v1/indexes/{index}/query"'*) ok "B reports the queries it served" ;;
  *) bad "no query counter in B's metrics" ;;
esac
grep -q 'pstore_blob_requests_total{class="list"} 0' <<<"$m" && ok "zero LISTs" || bad "a LIST happened"

# --- Criterion 7: the deployment document cannot fall behind the code. ------------------
# ⚠️ The extraction rule IS the criterion. Ids are the backticked token of a `### ` heading
# under `## Unscheduled duties`; anything else would make this grep match nothing and pass.
say "criterion 7 — docs/deploy.md matches /v1/admin/duties"
# ⚠️ `|| true` on both, and it weakens nothing: the `[ -n "$served" ]` guard below is what
# refuses an empty side. Without it, a `deploy.md` that LOST the heading would make `grep`
# exit 1, `pipefail` abort the script at the assignment, and the one line that says which side
# went missing never print -- a gate that fails closed but silently.
served=$(req "$A/v1/admin/duties" | body | grep -o '"id":"[a-z-]*"' | sed 's/.*:"//;s/"//' | sort || true)
documented=$(awk '/^## Unscheduled duties$/{s=1;next} /^## /{s=0} s' docs/deploy.md |
             grep -o '^### `[a-z-]\+`' | tr -d '#` ' | sort || true)
if [ -n "$served" ] && [ "$served" = "$documented" ]; then
  ok "$(echo "$served" | tr '\n' ' ')"
else
  bad "served [$(echo "$served" | tr '\n' ' ')] != documented [$(echo "$documented" | tr '\n' ' ')]"
fi

say "result"
[ "$fails" -eq 0 ] && { echo "byoc ok"; exit 0; }
echo "$fails failed"
exit 1
