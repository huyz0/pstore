# M123 — startup, shutdown, scaling and scalability

**Serves:** D-80 (the `batched` loss window is "up to T seconds"), D-86 (drain by
self-eviction), D-79 and C-6 (local rendezvous, a window growing with N), and the cost rules
in [AGENTS.md](../../../AGENTS.md). Four read-only reviews -- startup; shutdown and drain;
scaling and rebalancing; one node's scalability -- found what is below. Every finding and
its outcome: [FINDINGS.md](FINDINGS.md). No backlog row.

## What is true today

1. **A `batched` write is never flushed** -- on no timer, and not at shutdown. The only flush
   in the server is a `durable` write's. Every deploy loses every batched row written since
   the tenant's last durable write, and a tenant that only writes `batched` grows its
   memtable without bound.
2. **A graceful stop leaves its durable bundles unfolded and undue.** Due-ness lives in the
   stopped process's memory; its successor never stamps the predecessor's bundles, so
   `eventual` reads miss acknowledged writes until a `strong` read or an operator folds.
3. **Shutdown is unbounded.** The HTTP drain has no deadline, a second signal does nothing,
   and the replication worker finishes a whole tick of up to 64 tenants' copies after the
   stop.
4. **A stopping node keeps answering gossip** until it exits, and there is no readiness
   route, so peers keep assigning it shares through the drain and ~40 s after.
5. **A tenant's lane registry fails for good at 4,097 lanes**: `register` writes a set that
   `decode` refuses, and every fold, strong read and new writer of the tenant fails after.
   Every fold probes 8 bundles of every lane, idle or not.
6. **A split query places each segment over every server**: O(N) per segment, where C-6's
   window is O(log N + C); the `pstore-cluster` placement that implements it has no caller.
7. **Per-tenant request counters, stale fresh segments, and the eviction pass** grow with,
   or stall on, every tenant ever touched.
8. **Gossip and the roster never forget a dead node**; a booting node seeds them all alive.
9. **Startup:** the store-id read has no timeout or retry, the disk tiers open one after
   another, `pstore-node`'s announce has no timeout, and the replication worker claims its
   64 register shards one at a time.

## Delta

1. **Flush.** An engine reports its unflushed rows' age and approximate size, from memory.
   A flush loop, always on, flushes each tenant whose unflushed rows are older than
   `PSTORE_FLUSH_AGE_MS` (default 5 s) or larger than `PSTORE_FLUSH_BYTES` (1 MiB): one PUT
   per writing tenant per interval, never per index, never for an idle tenant.
2. **Stop.** On the signal: readiness fails and gossip leaves first; then the HTTP drain,
   bounded by `PSTORE_DRAIN_MS` (default 10 s); then every engine with unflushed rows is
   flushed, and every one with unfolded rows folded, together and bounded by the same
   deadline; then the cache closes. A second signal exits at once. The replication worker
   stops between tenants and releases its claims.
3. **Resume stamps.** A flush that resumes a lane past HEAD's watermark stamps what it found,
   so the predecessor's bundles are due.
4. **Leave.** A stopping member gossips its own death at a higher incarnation, which every
   peer accepts; `GET /v1/ready` answers 200 until the stop, 503 after.
5. **Lanes.** `register` refuses to grow a registry past `MAX_LANES` -- the write is refused,
   the tenant's registry stays readable. A clean stop retires its own lane from each tenant
   whose bundles of it are all folded. A fold's first probe of a lane is the one bundle at
   its watermark: an idle lane costs one HEAD, not eight.
6. **Placement.** A split query places a segment by `pstore-cluster`'s local rendezvous over
   the peer list, built once per refresh, with C-6's window.
7. **Scalability.** An evicted engine's request counters are dropped with it; a fold drops
   fresh segments of an older generation; the eviction pass decides idleness outside the
   registry's lock.
8. **Membership.** A member dead for longer than a retention period is forgotten; the roster
   drops entries dead past it; a booting node seeds only the roster's live entries.
9. **Startup.** The store-id read is bounded and retried with jitter; the three disk tiers
   open together; the announce is bounded; the 64 shards are claimed concurrently.

10. **The rest** (M123.5): the start scan reads and claims its shards together, the room
    shared out; a core serves before its disk tier recovers and attaches it after; a server
    may declare its AZ beside its URL and split within it (`PSTORE_PEER_GOSSIP_ZONE`); and a
    tenant's requests may go to its owner (`PSTORE_ROUTE_WRITES=on`), served where they land
    when it is down.

**Not changed:** any answer; any request on a path that succeeds, except as each delta
states; no LIST anywhere. Unset, the zone and routing change nothing.

## Acceptance criteria

Each a test, seen red before its fix and against a deliberate break of it; named in
[VERIFIED.md](VERIFIED.md) as each task lands.

1. A batched write is in the blob store within the flush age, and after a stop.
2. A successor's scheduled fold folds a stopped predecessor's bundles.
3. A stop with a request in flight past the deadline returns by the deadline, having flushed.
4. A stopped member is dead in a peer's view within one gossip round of its leave.
5. The 4,097th lane is refused and the registry still decodes; a clean stop retires its lane;
   a fold of a tenant with idle lanes probes one bundle a lane.
6. Placement of a segment touches `O(window)` servers, not N; moves ~1/N of segments on a
   join.
7. Counters of an evicted tenant are gone; a fold leaves no older fresh segment; the eviction
   pass holds the registry's lock for no `is_idle`.
8. A member dead past retention is forgotten, and the roster drops it.
9. Each startup bound holds against a store that never answers.
10. M119's bench: every digest unchanged. `./scripts/gates.sh` passes.
11. A 64-shard start scan is at most 4 rounds and holds the worker's bound; a core is
    `Opening` and serves before its tier attaches; a zoned server's list is its zone's; a
    routed tenant has one lane and is read-your-writes from every server, and is served
    where it lands when its owner is down.

## RA budget

Delta 1 adds one PUT per writing tenant per flush age -- the cost `durable` already paid per
write, now paid per interval. Delta 2 adds, at stop, one PUT per tenant with unflushed rows
and one fold per tenant with unfolded rows. Delta 5 removes 7 HEADs per idle lane per fold.
Nothing scales with indexes or records. No LIST.

## Tasks

- **M123.1** — Deltas 1, 2 (but leave), 3: the writes a stop or an idle tenant lost.
- **M123.2** — Deltas 4, 7.
- **M123.3** — Deltas 5, 6.
- **M123.4** — Deltas 8, 9.
- **M123.5** — Delta 10: the four findings M123.4 left kept, partial, or to the owner.
