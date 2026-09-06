#!/usr/bin/env bash
# Bring up an N-node pstore fleet, measure it, tear it down.
#
# ⚠️ Every wall-clock number this prints is `provisional`. N processes share one host bridge
# — microsecond latency, near-zero loss — and the dev VM is CPU-capped, so convergence
# measured in seconds is measuring Docker and the scheduler as much as gossip. The criteria
# M4b is gated on are counted in PROTOCOL PERIODS, which survive the transport.
#
#   scripts/cluster.sh up 100          bring up 100 nodes
#   scripts/cluster.sh up 100 0.1      ... with 10% probe loss injected (criterion 3)
#   scripts/cluster.sh converge        time until every node sees every node
#   scripts/cluster.sh cost            RSS and CPU per node
#   scripts/cluster.sh traffic 30      gossip bytes/s/node over a 30s window (criterion 5)
#   scripts/cluster.sh kill 5          kill 5 nodes, time detection
#   scripts/cluster.sh starve n7 90    starve one accuser for 90s (OQ-12, criterion 4)
#   scripts/cluster.sh down            containers AND the network -- see `down` for why
#
#   scripts/cluster-report.py 200      convergence and traffic, from the fleet's own logs
#
# ⚠️ Runs on HOST networking, not a bridge, because the host ARP table overflows at this
# fleet size and drops packets silently. dev/README.md has the measurement and the sysctl.
set -euo pipefail
cd "$(dirname "$0")/.."

NET=pstore-cluster
IMAGE=pstore-node
GOSSIP_PERIOD_MS=200
# ⚠️ Not 9000. In host networking the store binds a real host port, and 9000 is a common
# one — a collision surfaces as MinIO exiting with "port is already in use" while the nodes
# report only that the roster never answered.
MINIO_PORT=9400

case "${1:-}" in
up)
  N="${2:-100}"
  # Injected probe loss, applied in the node's gossip transport rather than with `tc netem`
  # — which would need NET_ADMIN on all N containers to test an unprivileged fleet.
  LOSS="${3:-0}"
  docker build -q -f dev/Dockerfile.node -t "$IMAGE" . >/dev/null

  # ⚠️ HOST networking, not a bridge, and the reason is a kernel limit rather than a
  # preference. A bridged container needs an ARP entry per node in the host's neighbour
  # table, whose default ceiling is `net.ipv4.neigh.default.gc_thresh3`. Past it the kernel
  # DROPS packets and logs `neighbour: arp_cache: neighbor table overflow!` — measured here,
  # joins stalled at ~33 of 100 nodes with the store idle, every stuck node sat in SYN_SENT,
  # and it read exactly like a roster or gossip bug. Raising the sysctl needs root on the
  # WSL2 host, which a dev container does not have; sharing one namespace needs nothing and
  # removes the entries rather than raising the ceiling.
  #
  # ⚠️ The cost, recorded because it changes what the numbers mean: the nodes are 100
  # PROCESSES on one loopback, not 100 network peers. Gossip no longer crosses a bridge, so
  # convergence and traffic here are a floor. `--memory` and `--cpus` still apply, so the
  # per-node cost figures are unaffected.
  base_port=7946
  # MinIO holds the roster. Only the roster: nodes own nothing, so there is no data to move.
  docker rm -f pstore-minio >/dev/null 2>&1 || true
  docker run -d --name pstore-minio --network host \
    -e MINIO_ROOT_USER=pstore -e MINIO_ROOT_PASSWORD=pstore-dev-secret \
    --memory 512m --cpus 1 \
    minio/minio:RELEASE.2025-04-22T22-12-26Z server /data --address ":$MINIO_PORT" >/dev/null
  sleep 3
  docker run --rm --network host --entrypoint sh minio/mc:latest -c \
    "mc alias set d http://127.0.0.1:$MINIO_PORT pstore pstore-dev-secret >/dev/null 2>&1 && \
     mc mb -p d/pstore >/dev/null 2>&1" || true

  echo "starting $N nodes..."
  for i in $(seq 1 "$N"); do
    # ⚠️ Capped per node. 100 unbounded containers is how a fleet test takes down the host
    # it is measuring, and the three nested ceilings in dev/README.md exist for this.
    port=$((base_port + i))
    docker run -d --name "pstore-n$i" --network host --memory 128m --cpus 0.1 \
      -e PSTORE_CLUSTER=c1 \
      -e PSTORE_PROBE_LOSS="${LOSS:-0}" \
      -e PSTORE_GOSSIP_ADDR="0.0.0.0:$port" \
      -e PSTORE_ADVERTISE="127.0.0.1:$port" \
      -e PSTORE_S3_ENDPOINT="http://127.0.0.1:$MINIO_PORT" \
      "$IMAGE" >/dev/null
    # A node joins from the roster, so the first few must seed it before the rest arrive.
    # ⚠️ Staggered. Starting 100 nodes in a tight loop puts 100 GETs and 100 conditional
    # PUTs on ONE key within a second: measured, 56 of 100 exited before the roster
    # answered. Real fleets do not start simultaneously, and the node backs off anyway —
    # this keeps the harness from manufacturing a herd the deployment would not have.
    sleep 0.15
    if [ "$i" -le 3 ]; then sleep 1; fi
  done
  echo "up: $(docker ps -q --filter name=pstore-n | wc -l) nodes"
  ;;

converge)
  N=$(docker ps -q --filter name=pstore-n | wc -l)
  echo "waiting for $N nodes to see $N members (periods of ${GOSSIP_PERIOD_MS}ms)"
  start=$(date +%s%3N)
  for _ in $(seq 1 120); do
    # The smallest view any node holds. Convergence is when the WORST node is complete —
    # an average would report success while a node still had a partial view.
    worst=$(docker ps -q --filter name=pstore-n | while read -r c; do
      docker logs --tail 1 "$c" 2>/dev/null | sed -n 's/.*members=\([0-9]*\).*/\1/p'
    done | sort -n | head -1)
    worst=${worst:-0}
    now=$(date +%s%3N)
    if [ "$worst" -ge "$N" ]; then
      ms=$((now - start))
      echo "converged: every node sees $N in ${ms}ms = $((ms / GOSSIP_PERIOD_MS)) periods (provisional)"
      exit 0
    fi
    sleep 1
  done
  echo "NOT converged; worst view was $worst of $N"
  exit 1
  ;;

cost)
  echo "per-node cost at $(docker ps -q --filter name=pstore-n | wc -l) nodes (provisional):"
  # ⚠️ Tab-separated and explicit. `{{.MemUsage}}` expands to "3.4MiB / 128MiB", so a
  # space-split awk reads the LIMIT as the next field — which reported every node at
  # "128% CPU" and looked like a fleet pegged at its cap. The harness was the bug.
  docker stats --no-stream --format '{{.MemUsage}}\t{{.CPUPerc}}' \
    $(docker ps -q --filter name=pstore-n) | awk -F'\t' '
      {split($1, m, " "); gsub(/MiB|%/, "", m[1]); gsub(/%/, "", $2);
       mem += m[1]; cpu += $2; n++}
      END {printf "  mean RSS %.2f MiB   mean CPU %.2f%%   total CPU %.1f%%   n=%d\n",
                  mem/n, cpu/n, cpu, n}'
  ;;

kill)
  K="${2:-5}"
  victims=$(docker ps --format '{{.Names}}' --filter name=pstore-n | head -"$K")
  echo "killing $K nodes"
  # SIGKILL, not stop: a clean shutdown would let the node announce its own departure, which
  # tests the goodbye path rather than the failure detector.
  for v in $victims; do docker kill "$v" >/dev/null; done
  N=$(docker ps -q --filter name=pstore-n | wc -l)
  killed_at=$(date -u +%Y-%m-%dT%H:%M:%S.%NZ)
  echo "$K killed; $N survivors must drop to $N"
  # ⚠️ Detection is timed from the survivors' own `VIEWCHANGE` lines, not from polling.
  # Polling once a second resolves to five gossip periods, and a criterion counted in
  # periods cannot be judged by an instrument coarser than the thing it measures.
  for _ in $(seq 1 120); do
    done_n=$(docker ps -q --filter name=pstore-n | while read -r c; do
      docker logs "$c" 2>/dev/null | grep -c "VIEWCHANGE members=$N "
    done | grep -c '^[1-9]')
    if [ "$done_n" -ge "$N" ]; then
      python3 scripts/cluster-report.py --detect "$killed_at" "$N" "$GOSSIP_PERIOD_MS"
      exit 0
    fi
    sleep 1
  done
  echo "NOT detected; only $done_n of $N survivors dropped to $N"
  exit 1
  ;;

traffic)
  # Gossip bytes/s/node, counted at the chitchat socket. ⚠️ NOT `docker stats` network
  # counters: those include the blob store and the container runtime's own traffic, which at
  # 25 nodes is most of what they show.
  W="${2:-30}"
  N=$(docker ps -q --filter name=pstore-n | wc -l)
  sample() {
    docker ps -q --filter name=pstore-n | while read -r c; do
      docker logs "$c" 2>/dev/null | grep -o 'sent=[0-9]* recvd=[0-9]*' | tail -1 |
        tr -d 'sentrecvd=' 
    done | awk '{s += $1 + $2} END {print s + 0}'
  }
  a=$(sample); sleep "$W"; b=$(sample)
  echo "$N $a $b $W" | awk '{printf "gossip: %.0f bytes/s/node at %d nodes (provisional)\n", ($3-$2)/$4/$1, $1}'
  ;;

starve)
  # ⚠️ OQ-12: a node whose OWN probe loop is starved must not evict healthy peers.
  # The first draft of this test starved a *target*, which is indistinguishable from death —
  # a correct detector should evict it, so the test could only pass against a broken one.
  # What is measured here is the starved node's own view of everyone else.
  V="${2:-pstore-n7}"
  N=$(docker ps -q --filter name=pstore-n | wc -l)
  before=$(docker logs --tail 1 "$V" 2>/dev/null | sed -n 's/.*members=\([0-9]*\).*/\1/p')
  echo "starving $V (view $before of $N)"
  # A control node, to tell "the accuser was starved" from "the whole host was busy".
  C=$(docker ps --format '{{.Names}}' --filter name=pstore-n | grep -v "^$V\$" | head -1)
  v0=$(docker logs "$V" 2>&1 | grep -c '^VIEW')
  c0=$(docker logs "$C" 2>&1 | grep -c '^VIEW')
  docker update --cpus 0.01 "$V" >/dev/null
  # A quota alone only bites under demand, so give it demand: without a hog the node stays
  # idle enough to meet its deadlines and the test measures nothing.
  docker exec -d "$V" sh -c 'while :; do :; done' 2>/dev/null || true
  sleep "${3:-90}"
  # ⚠️ Evidence that the starvation BIT, printed rather than assumed. A `--cpus` quota that
  # failed to apply, or a hog that never started, leaves a node running at full speed — and
  # then "a starved accuser evicted nobody" is a sentence about a node that was never
  # starved. Ticks are the node's own loop rate, so falling behind the control is the
  # observable that says the deadline was missed.
  vt=$(( $(docker logs "$V" 2>&1 | grep -c '^VIEW') - v0 ))
  ct=$(( $(docker logs "$C" 2>&1 | grep -c '^VIEW') - c0 ))
  after=$(docker logs --tail 1 "$V" 2>/dev/null | sed -n 's/.*members=\([0-9]*\).*/\1/p')
  peers=$(docker ps -q --filter name=pstore-n | while read -r c; do
    docker logs --tail 1 "$c" 2>/dev/null | sed -n 's/.*members=\([0-9]*\).*/\1/p'
  done | sort -n | head -1)
  echo "starved accuser: view ${before:-?} -> ${after:-?} of $N; worst peer view $peers"
  docker update --cpus 0.1 "$V" >/dev/null
  docker exec "$V" sh -c 'kill -9 $(pidof sh) 2>/dev/null' >/dev/null 2>&1 || true
  echo "starvation evidence: accuser advanced $vt ticks, control $ct"
  if [ "$ct" -eq 0 ] || [ $(( vt * 100 / ct )) -gt 90 ]; then
    echo "OQ-12: VACUOUS — the accuser kept pace with the control, so it was never starved"
    exit 1
  fi
  # It must not have evicted the healthy fleet. Losing a couple to its own slowness is the
  # detector working; dropping most of them is the false-positive storm OQ-12 asks about.
  if [ "${after:-0}" -ge $(( N * 9 / 10 )) ]; then
    echo "OQ-12: PASS — a starved accuser kept ${after} of $N"
  else
    echo "OQ-12: FAIL — a starved accuser evicted down to ${after} of $N"
    exit 1
  fi
  ;;

down)
  docker rm -f $(docker ps -aq --filter name=pstore-n) pstore-probe pstore-minio >/dev/null 2>&1 || true
  # ⚠️ Remove the NETWORK too, not just the containers. Measured on WSL2: after a few
  # up/down cycles of ~100 containers, the bridge stops giving NEW containers any
  # connectivity while EXISTING ones keep working — a joining node then times out on a
  # blob store that its neighbours are reading successfully, which reads exactly like a
  # gossip or roster bug and is neither. A reused bridge made every run after the first
  # measure a degraded network.
  docker network rm "$NET" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  echo "down"
  ;;
*)
  sed -n '3,12p' "$0"; exit 1 ;;
esac
