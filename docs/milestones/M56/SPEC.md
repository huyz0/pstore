# M56 — The peer list from membership

**Serves:** [BACKLOG](../BACKLOG.md) row 58, which the project owner asked on 2026-10-06 to
close with the rest of the backlog. It builds on [M54](../M54/SPEC.md) (the split) and the
SWIM membership of M50–M53.

## What is true today

Read from the tree at M55, nothing measured:

- **A server's peers are a static list** (`PSTORE_PEERS`, `PSTORE_PEER_SELF`, M54). Adding or
  removing a server means restarting every other one.
- **A dead peer is tried on every query.** Each query sends it its share, waits up to
  `PSTORE_PEER_TIMEOUT_MS`, then runs the share itself. That costs two more rounds plus the
  timeout, for as long as the peer is in the list.
- **Membership exists, in another process.** `pstore-node` runs SWIM (`pstore_node::swim`):
  - it is sealed under a cluster key (M53);
  - it declares a silent member dead after about 14 periods (`pstore-node/src/lib.rs`: probe
    wait, then a 3-period direct timeout, a 3-period indirect timeout, and a suspicion window of
    max(6, 3·log2 N) periods);
  - the period is an argument of `swim::start_with`.

  `pstore-server` does not depend on it.
- **A SWIM member carries an address and a zone.**
  - The address is the UDP `host:port` it advertises, or the source address a probe first saw
    it at (`Protocol::learn`).
  - The zone is a free string the member declares. An empty zone means "unknown". Gossip fills
    an empty zone and never moves a known one within an incarnation (M24).
  - `Member::members_zoned` returns every member not declared dead, suspects included, with its
    zone.
- **A member restarts at incarnation 0** (`Cluster::new`), and its identity is derived from its
  advertise address. Suppose a member restarts faster than the others detect, and declares a
  new zone. The others still hold it Alive at incarnation 0, and a zone never moves at an
  equal incarnation, so its old zone stays in their views until something suspects it (spec
  review).
- **A member cannot be stopped.** Its receiver and ticker are detached tasks
  (`swim::spawn_receiver`, `spawn_ticker`), and dropping the `Member` leaves them running. The
  protocol has no leave message.

## Delta

**A server may take its peers from a SWIM membership it runs itself. Its member declares the
server's own HTTP URL as its zone. The peer list is every member whose zone is a server URL,
plus the server itself. A background task refreshes it once per period, and each query takes
one snapshot of it. When a server joins, it is assigned segments as soon as the others' views
hold it. When a server dies, it stops being sent shares once they declare it dead.**

1. **Configuration**, as an alternative to `PSTORE_PEERS`:
   - `PSTORE_PEER_SELF`: this server's HTTP URL, as at M54 (`http://`, no path). It is what
     the member declares.
   - `PSTORE_PEER_GOSSIP_ADDR`: the UDP `ip:port` to listen on.
   - `PSTORE_PEER_GOSSIP_ADVERTISE`: the `ip:port` to advertise. Defaults to the listen
     address.
   - `PSTORE_PEER_GOSSIP_SEEDS`: `ip:port`s, comma-separated. May be empty for the first
     server.
   - `PSTORE_PEER_GOSSIP_PERIOD_MS`: the SWIM period. Default 1,000.
   - The gossip key, read by `pstore-node`'s own functions (`schedule::gossip_key_source`,
     `gossip_keys`): `PSTORE_GOSSIP_KEY` or `PSTORE_GOSSIP_KEY_FILE`, or
     `PSTORE_GOSSIP_INSECURE=1`, with the same refusals.
   - `PSTORE_PEER_TIMEOUT_MS` applies as at M54.
   - Every gossip address is normalised through a `SocketAddr` parse and re-display, so two
     spellings of one IP are one member.
   - **Refused:**
     - `PSTORE_PEER_GOSSIP_ADDR` set together with `PSTORE_PEERS`;
     - a gossip address without `PSTORE_PEER_SELF`;
     - `PSTORE_PEER_GOSSIP_ADVERTISE`, `_SEEDS` or `_PERIOD_MS` without
       `PSTORE_PEER_GOSSIP_ADDR`;
     - a listen, advertise or seed address that is not an IP literal with a port. Hostnames
       are refused because a member first learned by probe keeps its source address, so a
       name and its IP would be two members;
     - an advertise address whose IP is unspecified (`0.0.0.0`, `::`) or whose port is 0. It
       is unroutable, and every server would derive the same identity from it;
     - a zero or non-numeric period;
     - a missing key without `PSTORE_GOSSIP_INSECURE=1`.
2. **The list** is a pure function of the view's `(address, zone)` pairs and this server's
   URL:
   - Members whose zone parses as a peer URL (`http://`, no path, as M54 checks) are in.
   - Every other member is out: a `pstore-node` process sharing the key, and a member whose
     zone is still unknown.
   - This server's URL is always in.
   - URLs are deduplicated and sorted. `me` is this server's position.
   - IPv6 URLs keep their brackets, because the URL is the declared string, never assembled
     from an address.
3. **Refresh and snapshot.**
   - A task reads `members_zoned` once per period and swaps in a new `Arc` of the list.
   - Only the list and `me` are swapped. The HTTP client and the counters
     (`pstore_peer_parts_sent`, `_failed`) are built once and survive every refresh.
   - A query takes the current `Arc` once, when it builds its `QueryPeers`. Its `servers`, `me`,
     `part` and `phased` then all read the same list, however the view moves during the query.
   - `assign` is M54's rendezvous hash over that list, so a server joining or leaving moves
     only its own share of segments.
4. **A dead member** is gone from the list at the first refresh after the view declares it.
   Until then its shares fail and run on the coordinator, as M54's do.
5. **Membership is a cache hint, never correctness.** Two coordinators whose views differ for
   a moment may assign a segment differently. Both answers are exact, because any server can
   scan any segment.
6. **Stop and restart** (task M56.1, in `pstore-node`):
   - `Member::stop` aborts its receiver and ticker tasks, and dropping a `Member` does the
     same. Its socket is released. A stopped member falls silent, and the others declare it
     dead within the detection bound. There is still no leave message.
   - A member starts at incarnation `now_micros()`, not 0. A restart then outranks every record
     the others hold of it, so a zone it changed across the restart replaces the old one, as
     the zone rules already allow at a higher incarnation. This is `pstore-node`'s behaviour
     too, and it fixes the same quick-restart case there.
7. **Observable:** a gauge `pstore_peer_servers`, the length of the current list (1 when the
   server is alone or unpeered), and a `#[doc(hidden)]` accessor for the list, for tests.

**Not changed:** the query path of M54–M55, the gossip protocol and wire format, and
`pstore-node`'s behaviour other than M56.1. ⚠️ M56.1 changes it: stop, and the starting
incarnation.

**Not covered, and the ledger says so:**
- **Seeds are static.** `pstore-node` reads its seeds from the roster in the bucket. A server
  doing the same is a later milestone.
- **There is no graceful leave.** A server that stops is found by timeout. Until then every
  query that assigns it a share pays M54's failure cost.
- **The zone field is borrowed.** A server's member declares a URL where a node declares an
  AZ. A server in a fleet that also runs `pstore-node` on the same key is in that fleet's
  member table. Nodes never place work on it: a node's roster cell holds only members of its
  own zone, and no node's zone is a URL. But it is counted in the node's `member_count`.

## Acceptance criteria

1. **Configuration.** The pure parser:
   - accepts gossip with `PSTORE_PEER_SELF` and a key, with the default and an explicit period;
   - refuses each case in rule 1;
   - given none of it, leaves M54's static list as the only way to have peers.

   Test: `gossip_peers_are_configured_whole_and_never_beside_a_list`.
2. **The list from a view.** Pure, over hand-written `(address, zone)` pairs. The result is
   sorted and deduplicated, with `me` at this server's position:
   - server members are in;
   - a member with an AZ zone and one with an empty zone are out;
   - two members declaring one URL appear once;
   - this server is in when its own member is absent;
   - an IPv6 URL keeps its brackets.

   Test: `the_peer_list_is_the_servers_in_the_view`.
3. **Stop** (M56.1).
   - Two members are sealed on loopback at a 100 ms period. One is stopped.
   - Within 40 periods the other no longer counts it alive.
   - After the stop, its bytes sent and received stay flat over 10 periods, and its UDP port
     can be bound again.

   Test: `a_stopped_member_falls_silent_and_is_declared_dead`, in `pstore-node`.
3b. **A quick restart moves its zone.** Two members, A and B, at a 100 ms period. B is stopped
   and restarted at once on the same address, declaring a different zone, before A could
   suspect it. Within 40 periods, A's view holds B with the new zone. Test:
   `a_member_restarted_with_a_new_zone_is_believed`, in `pstore-node`.
4. **A fleet that finds itself.**
   - Three servers on loopback, each on its own HTTP port.
   - Each has a sealed gossip member at a 100 ms period and no static list. The second and
     third are seeded with the first.
   - Within 40 periods every server's list names all three, and `pstore_peer_servers` reads 3.
   - Then a dense query and a text query, each asked of each server, split
     (`pstore_peer_parts_sent` rises) and answer equal to an unpeered server.

   Test: `servers_take_their_peers_from_membership`, in `crates/pstore-server/tests/peers.rs`.
5. **A server that leaves stops being sent shares.** In the same fleet (whose segments are
   checked with `assign` to give every server some), one server's member and HTTP listener are
   both stopped:
   - before the others' lists drop it, a query's share to it fails, and
     `pstore_peer_parts_failed` rises;
   - within 40 periods both other lists drop it;
   - over 10 queries after that, `pstore_peer_parts_failed` stays flat on both. Queries still
     split between the two (`pstore_peer_parts_sent` rises);
   - every answer, before and after, equals the unpeered server's.

   Test: `a_server_that_leaves_is_sent_nothing`.
6. **Gates:**
   - `./scripts/gates.sh` passes;
   - the incremental mutation sweep of the changed lines misses 0;
   - `cargo deny check` passes its licence and ban checks with `pstore-node` a dependency of
     `pstore-server`.

## Test plan

| AC | Seen red first under |
|---|---|
| 1 | each refusal removed in turn; the both-set check removed |
| 2 | the zone filter removed (an AZ member in the list); dedup removed; self not inserted |
| 3 | `stop` that aborts nothing |
| 3b | the starting incarnation left at 0 |
| 4 | the refresh task never run (list stays `[self]`, nothing splits) |
| 5 | a list that never drops dead members (the failure count keeps rising) |

## RA budget

Blob requests unchanged: W, Rseq, Rpar, List and depth as at M55. The added traffic is SWIM's
own: per server, one probe per period, plus its acks and its piggybacked gossip.

## Risks

- **A suspect stays in the list** for up to max(6, 3·log2 N) periods. Each query that assigns it
  a share pays M54's timeout. This is revealed by `pstore_peer_parts_failed` rising during the
  window, and is bounded by the detection time.
- **`pstore-server` gains `pstore-node`'s dependencies**, `chitchat` and `ring` among them.
  `cargo deny` reveals a licence or advisory problem. Moving `swim` into `pstore-gossip` is
  the fix if they ever conflict.
- **A wrong `PSTORE_PEER_SELF`** puts an unreachable URL in every list, and that share fails
  on every query. It shows as `pstore_peer_parts_failed` rising, as it would for a wrong
  static list.

## Tasks

- **M56.1** `Member::stop`, stop on drop, and the starting incarnation: AC3, AC3b.
- **M56.2** Configuration, the list function, the refresh task, the per-query snapshot, the
  gauge, and wiring in `main`: AC1, AC2, AC4, AC5.
