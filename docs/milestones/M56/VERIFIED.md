# M56 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. Every member and server runs on loopback
in one process. Periods of 100 ms bound the tests in periods, never in seconds of a real
network.

Commands:
- `cargo test -p pstore-node --test swim`: 18 tests, the three of M56.1 among them.
- `cargo test -p pstore-server --test peers`: 21 tests, the five of M56.2 among them.

⚠️ **For M56.2, the tests came first but could not compile until the code existed.** So each
was then **seen red under a hand mutation** of the code it checks, named on its line. M56.1's
tests were seen red against a `stop` that did nothing, before `stop` was written. The
restart test was seen red at incarnation 0, before the clock was used.

1. **Configuration.** `gossip_peers_are_configured_whole_and_never_beside_a_list`:
   - Accepted: gossip with `PSTORE_PEER_SELF` and a key, with the default period, advertise and
     timeout, and with each set explicitly. IPv6 addresses are re-displayed, so two spellings
     are one. `PSTORE_GOSSIP_INSECURE=1` gives no key.
   - With gossip set, `PeerConfig::from_vars` gives no static list and no longer refuses a lone
     `PSTORE_PEER_SELF`. Without gossip it still refuses one.
   - With none of it: no gossip, and M54's list still parses.
   - 16 cases refused:
     - beside `PSTORE_PEERS`;
     - without `PSTORE_PEER_SELF`, and with an `https://` one;
     - each of `_ADVERTISE`, `_SEEDS`, `_PERIOD_MS` without `_ADDR`;
     - a hostname to listen on, to advertise, or as a seed;
     - an unspecified v4 or v6 advertise address, and port 0;
     - a period of 0 or not a number;
     - no key, and a bad key.
   - Killed by hand: the refusal beside a list removed; the unspecified refusal removed.
2. **The list from a view.** `the_peer_list_is_the_servers_in_the_view`:
   - an AZ zone, an empty zone, an `https://` zone and a zone with a path are out;
   - two members declaring one URL appear once, a trailing `/` included;
   - this server is in, at `me`, whether or not its own member is in the view;
   - alone, it is the only entry;
   - an IPv6 URL keeps its brackets.

   Killed by hand: the zone filter removed; the dedup removed; this server not inserted.
3. **Stop** (M56.1). `a_stopped_member_falls_silent_and_is_declared_dead`:
   - two members, sealed, at a 100 ms period;
   - once one is stopped, the other no longer counts it within 40 periods;
   - its bytes sent and received stay flat over 10 periods, and its port binds again.

   Added from the sweep, `a_dropped_member_falls_silent_too`: a member dropped without `stop`
   is declared dead too. Killed by hand: `Drop` doing nothing.
3b. **A quick restart moves its zone** (M56.1).
   `a_member_restarted_with_a_new_zone_is_believed`: B is stopped and restarted at once on its
   address under a new zone. Within 40 periods A holds the new zone. Red at incarnation 0.
4. **A fleet that finds itself.** `servers_take_their_peers_from_membership`:
   - three servers on loopback, each on its own HTTP port, with sealed members and no static
     list;
   - within 40 periods every list names all three, and `pstore_peer_servers` reads 3, or 1 on
     an unpeered server;
   - a dense query and a text query, asked of each server, split and equal the unpeered
     server's, with no failed part.
   - Added in code review: a second `join_peers`, or one after `set_peers`, is refused.
   - Killed by hand: the refresh never applied (the list stays this server alone); the
     once-only refusal removed; the membership's `Debug` showing nothing.
   - Added from the sweep, `a_dropped_membership_leaves_the_list_too`: dropping the handle,
     not only `stop`, ends the member, and the other server's list drops it within 40 periods.
     Killed by hand: `PeerGossip`'s `Drop` doing nothing.
5. **A server that leaves stops being sent shares.** `a_server_that_leaves_is_sent_nothing`:
   - the fixture's segments are checked with `assign` to give every server some;
   - the third server's member stops, and its HTTP shutdown is awaited;
   - a query just after fails that server's share, which runs here, and the answer is equal;
   - within 40 periods both other lists drop it;
   - over 10 rounds of queries from each remaining server, every query splits, every answer is
     equal, and `pstore_peer_parts_failed` does not move;
   - `pstore_peer_servers` reads 2.

   Killed by hand: a refresh that never drops a member it once listed.
6. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit, and on M56.1's.
   - `cargo deny check licenses bans sources`: passes with `pstore-node` a dependency of
     `pstore-server`. ⚠️ Advisories fail on `paste` (RUSTSEC-2024-0436) through `foyer`, as
     at M53 to M55: not this change.
   - **Mutation**, as M54 and M55 ran it (`--no-config --profile mutants`, `-j 1`):
     - M56.1, `pstore-node/src/swim.rs` against `--test swim`: **11 mutants, 1 missed**, 1
       caught, 9 unviable;
     - M56.2, `pstore-server/src/peers.rs` and `lib.rs` against the server's unit tests and
       `--test peers`: **57 mutants, 2 missed**, 43 caught, 12 unviable.
   - **Each of the 3 misses now has a test that kills it, seen by hand:**
     - `Member`'s `Drop` (M56.1): `a_dropped_member_falls_silent_too`;
     - `PeerGossip`'s `Drop`: `a_dropped_membership_leaves_the_list_too`;
     - `PeerGossip`'s `Debug`: `servers_take_their_peers_from_membership` checks what it
       shows.
   - ⚠️ The M56.2 sweep ran before code review's last fixes (the once-only refusal, the
     advertise variable's name, the awaited shutdown). Those lines were checked by the hand
     mutations above, not swept.

**Not covered**, as the spec states:
- seeds are static;
- there is no graceful leave: a server that stops is sent shares, which fail and run here,
  until it is declared dead, about 14 periods;
- the zone field is borrowed: a server sharing a key with `pstore-node` is in that fleet's
  `member_count`;
- a clock stepped back past a member's old incarnation leaves its old record winning, as 0
  always did. The fix is in the gossip protocol (`absorb`), not this milestone (code review of
  M56.1);
- ⚠️ with `PSTORE_GOSSIP_INSECURE=1` any datagram can add a URL to every list (code review),
  so `docs/deploy.md` warns against it beside the other risks.
