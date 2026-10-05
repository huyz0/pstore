# M53 — Gossip authenticated

**Serves:** the gossip residue the project owner asked to address on 2026-10-05: "authenticate
gossip". [M44](../M44/SPEC.md), [M49](../M49/SPEC.md) and [M52](../M52/VERIFIED.md) each left
"gossip trusts its network".

## What is true today

Read from the tree at M52, nothing measured:

- **Any datagram is believed.** `swim.rs`'s receiver decodes whatever arrives and hands it to
  `Protocol::receive` with its UDP source address.
  - `learn` joins any unknown `from` id at that address, so one datagram adds a member.
  - Answers go to the source address, which any sender can claim. M44 measured a forged digest
    drawing about 260 KB from a 3,000-member node; M49 bounds that to a budget a tick.
- **A replay is believed too.** A captured `Ping` or `Part` re-sent later is processed again,
  and its answer goes to whatever source address the replay claims.
- **A message's `from` is not always its sender.** A relayed indirect-probe `Ack` carries the
  probed target's id but is sent by the relay (protocol.rs, the `relaying` path).
- **Two tests hard-code the datagram size**:
  - `a_part_of_exactly_the_datagram_is_one_part` uses the literal 65,507;
  - `answers_that_exactly_fill_the_budget_are_sent` sizes two addresses (65,344 and 65,452)
    from it.
- `ring` 0.17 is already in `Cargo.lock`, through `quinn` and `rustls`. Its licence, "Apache-2.0
  AND ISC", is already allowed by `deny.toml`.

## Delta

**A SWIM node seals every datagram with HMAC-SHA256 under a cluster key, and drops, before the
protocol sees it, any datagram whose seal fails, is stale, or is a replay. A node with no key
refuses to start unless it is told explicitly to run unauthenticated.**

1. **The frame:** `payload || sealer || dest || epoch || counter || time || tag`, an 88-byte
   seal (`SEAL`):
   - `sealer`: the sending node's 16-byte id, the one the protocol uses for itself;
   - `dest`: the 16-byte id of the node it is for (rule 4);
   - `epoch`: 8 bytes, the sender's microseconds since the Unix epoch when its transport
     started. A restart is a new epoch;
   - `counter`: 8 bytes, starting at 0 in each epoch, one more for every datagram sealed;
   - `time`: 8 bytes, the sender's microseconds since the Unix epoch when sealing;
   - `tag`: the full 32-byte HMAC-SHA256 of `b"pstore-gossip-1"` followed by everything before
     the tag. The label means a tag made for another use of the key never verifies here.
   - All integers little-endian. Verified with `ring::hmac::verify`, which is constant-time,
     before any other field is read.
2. **Freshness:** a `time` more than 60 s from the receiver's clock, either way, is refused.
3. **Destination:** a frame whose `dest` is not the receiver's own id is refused. Without it,
   a datagram captured on its way to X and replayed to Y within 60 s would carry a counter Y
   has never seen, and be accepted (spec review).
4. **How a sender names `dest`**, from what it holds when it sends:
   - a reply to the source address of a datagram just opened is for that datagram's `sealer`,
     verified by the seal. That holds when the observed source differs from the sender's
     advertised address (NAT, a `0.0.0.0` bind);
   - any other datagram is for the member the view holds at that address;
   - failing that, `derive_id(address)`, as seeds are joined today.
   - ⚠️ So a node dialled by an address other than the one it advertises cannot be reached by
     a keyed fleet until it dials first. That is the assumption `derive_id` already makes for
     seeds.
5. **Replay**, keyed by `(sealer, epoch)`, which the tag vouches for. The `from` inside the
   payload is not used: a relayed `Ack` names another node.
   - The receiver keeps the highest counter seen and a 64-bit window that includes it, as
     IPsec does.
   - A counter above the highest is accepted and moves the window.
   - One 1 to 63 below it is accepted once.
   - 64 or more below it is refused, and so is a repeat.
   - **Each datagram is sealed immediately before its own `send_to`**, never a batch sealed
     ahead. So datagrams reordered by up to 63 sends are accepted, including the receiver and
     ticker tasks sealing for the same peer concurrently.
   - **At most 2 live epochs a sealer.** A third replaces the one with the oldest newest
     `time`. A restart overlaps its old epoch, which no honest sender needs more than that.
   - **An epoch floor a sealer:** the highest epoch evicted so far. An epoch at or below it
     is refused, so a datagram of an evicted epoch, still fresh, cannot open a new entry
     (spec review). Epochs are start times, so they rise across restarts. The floor goes with
     its sealer's entry.
   - **Eviction:** an entry whose newest `time` is more than 60 s old is removed. Freshness
     already refuses anything that entry could match, so this cannot admit a replay. The sweep
     runs at most once a second.
   - So the table holds at most 2 entries for each sealer heard from in the last minute. For
     honest senders that is bounded by the fleet; a key holder can mint sealer ids (Not
     covered).
6. **Room for the seal:**
   - `pstore-gossip`'s `MAX_DATAGRAM` becomes 65,507 − `SEAL` = 65,419, with `SEAL` = 88 public
     beside it.
   - M44's splitting follows it, and so does M49's budget: 4 × 65,419 = 261,676.
   - The receive buffer in `swim.rs` becomes `MAX_DATAGRAM + SEAL` = 65,507. Left at
     `MAX_DATAGRAM`, the kernel would truncate a maximal sealed datagram, its seal would fail,
     and it would be refused (spec review).
   - The two tests above derive their sizes from `MAX_DATAGRAM`, keeping their boundaries:
     - a `Part` of exactly `MAX_DATAGRAM` bytes is one `Part`, and one byte more is two;
     - two answers of exactly half the budget are both sent, and the next is not.
     - Their assertions do not change.
   - M44's, M49's and M52's specs quote 65,507 and 262,028, and get a correction banner.
7. **Counted:** a datagram refused for any of these reasons adds to a `refused` count, read by
   a new `Member::refused()`. `traffic()` keeps its three fields: chitchat shares them.
8. **Configuration:**
   - `PSTORE_GOSSIP_KEY`: one or two keys, hex-encoded, comma-separated, each at least 32
     bytes (64 hex digits). The node seals with the first and accepts either. **Rotation**
     rolls through the fleet in three passes: add the new key second, move it first, drop the
     old one.
   - `PSTORE_GOSSIP_KEY_FILE`: the same value read from a file, trailing whitespace ignored.
     Setting both refuses to start.
   - **Neither set: the node refuses to start**, unless `PSTORE_GOSSIP_INSECURE=1`, exactly;
     any other value, `true` included, refuses to start. With it, the node runs
     unauthenticated, as today, and says so in one line at start.
   - A key with `PSTORE_GOSSIP=chitchat` refuses to start: chitchat cannot honour it.
   - A malformed key (odd length, non-hex, under 32 bytes, more than two) refuses to start.
   - `scripts/cluster.sh` passes a fixed development key to SWIM nodes, and
     `PSTORE_GOSSIP_INSECURE=1` to chitchat ones. `docs/deploy.md` documents all three
     variables.
   - ⚠️ **Turning the key on in a running unauthenticated fleet splits it until every node has
     restarted.** A keyed node and an unkeyed one refuse each other.

**Not changed:** the protocol, its messages, the `Sim`, chitchat.

**Not covered, and the ledger says so:**
- **The source address is not authenticated.** A sealed datagram's answer still goes to its
  UDP source. An on-path attacker can rewrite the source of a first copy before it arrives.
  A replay is refused, at its destination and at any other node.
- **Any key holder is trusted.** It can mint sealer ids and epochs, growing the replay table for
  60 s at a time, and it can seal under another node's `sealer` id, and push that
  id's replay window forward, refusing its datagrams for up to 60 s. That is inherent to a
  shared key.
- **A node whose clock is more than 60 s off cannot gossip.** A restarted node is not locked
  out: its new epoch is a new window.
- **No confidentiality.** Membership is not secret, and datagrams stay readable.

## Acceptance criteria

1. **The seal holds.** `pstore-node` unit tests on the pure seal functions, at a fixed clock:
   - a sealed payload opens to itself under the same key, and under a second accepted key;
   - it is refused under another key;
   - it is refused with any single byte flipped, at every position of a 100-byte payload's
     sealed frame;
   - shorter than 88 bytes, it is refused;
   - sealed for another `dest`, it is refused, though its seal is valid.
   - **Golden vector:** for a fixed key, payload, sealer, epoch, counter and time, the frame
     equals a hex literal computed outside the crate (Python's `hmac`, recorded in the ledger),
     with the label written as a literal in the test. A change to the label, field order or
     width fails it.
   - Test: `seal::tests::a_seal_opens_only_unaltered_under_its_key`.
2. **Freshness.** A `time` 60 s either side of the receiver's clock opens, and 60 s plus 1 µs
   does not. Test: `seal::tests::a_stale_seal_is_refused`.
3. **Replay.** For one `(sealer, epoch)`:
   - each counter is accepted once;
   - counters arriving out of order, 1 to 63 below the highest, are accepted once each, 63
     included;
   - 64 below the highest is refused;
   - a third epoch of one sealer evicts the oldest of its two, and a datagram of the evicted
     epoch is then refused, as is any epoch below it;
   - a second epoch of the same sealer has its own window, so a restart is accepted;
   - two sealers sealing the same `from` (a relayed `Ack`) do not share a window.
   - Test: `seal::tests::a_replay_is_refused_and_reordering_within_the_window_is_not`.
3b. **`dest` is chosen by rule 4.** A pure test of the choice:
   - a reply to the source of a datagram sealed by S, from an address whose `derive_id` is
     not S, is for S;
   - a datagram to an address the view holds is for that member;
   - one to an unknown address is for `derive_id` of it.
   - Test: `seal::tests::a_reply_is_sealed_for_its_verified_sender`, or the transport's
     equivalent.
4. **The table shrinks.** An entry whose newest `time` is more than 60 s old is evicted by the
   next sweep. An entry within 60 s is kept, and its replays are still refused. Test:
   `seal::tests::a_quiet_sender_is_forgotten_after_the_window`.
5. **The fleet.** Real nodes on loopback, over UDP, in `crates/pstore-node/tests/swim.rs`:
   - with the same key, two nodes find each other, as `two_members_find_each_other_from_a_seed`
     does unkeyed. Test: `keyed_members_find_each_other`;
   - with different keys, the seed node's view stays at 1 for 2 s, and its `refused()` is above
     0. Test: `a_member_with_another_key_is_refused`;
   - a keyed node refuses an unkeyed one, and an unkeyed node refuses a keyed one. Test:
     `keyed_and_unkeyed_members_refuse_each_other`;
   - a node holding the old and new keys gossips with a node holding only the old one. Test:
     `a_second_key_lets_a_rotation_roll`;
   - **the largest datagram arrives:** a sealed `Part` of exactly `MAX_DATAGRAM` bytes,
     65,507 sealed, sent from a raw socket and sealed for that node, adds its member to a keyed node's view. Test:
     `the_largest_sealed_part_is_received`.
6. **Configuration refuses what it cannot honour.** The pure parser:
   - accepts one key and two keys of 32 bytes;
   - refuses 31 bytes, odd length, non-hex, three keys, both variables set, a key with
     chitchat, and neither set without `PSTORE_GOSSIP_INSECURE=1`;
   - accepts neither set with it, and refuses `PSTORE_GOSSIP_INSECURE=true` and `=0`.
   - Test: `a_gossip_key_is_required_well_formed_and_swim_only`, in the crate's config tests.
   - **The file:** a key file with a trailing newline gives the same keys as the variable, and
     an unreadable path refuses to start. Test: `a_gossip_key_file_is_read_and_trimmed`.
7. **The boundaries hold at the new size.** The two recalibrated tests pass, and each was seen
   red with its size moved one byte the wrong way.
8. **Nothing else moves beyond a bound.** `./scripts/gates.sh` passes. In
   `./scripts/gossip-loss.sh`, every figure is within 1% of M52's ledger, and no bound moves.
   The smaller `MAX_DATAGRAM` only adds `Part` headers, so no figure should fall.
9. **Mutation:** the incremental sweep of the changed lines misses 0
   (`--no-config --profile mutants`). `./scripts/mutants.sh`.
