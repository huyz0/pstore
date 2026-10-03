# M44 — A `Part` that fits a datagram

**Serves:** [BACKLOG](../BACKLOG.md) row 53's residue, which [M43](../M43/VERIFIED.md) narrowed:
"a `Part` is uncapped".

## What is true today

- A `Digest` or `TaggedDigest` is answered with **one** `Part` holding every member of the
  buckets or leaves that differ. Nothing bounds its size.
- `pstore-node`'s SWIM loop reads into a 65,507-byte buffer (`MAX_DATAGRAM`, swim.rs). It sends
  with `socket.send_to(&msg.encode(), …)` and ignores an error. A `Part` above 65,507 bytes
  fails `send_to` and **is never seen by anyone**. Nothing counts or logs it.
- A member costs 33 bytes plus its address and zone (about 60 to 70 bytes). So about 1,000
  members in one `Part` is past the limit. Under heavy loss, M43 measured most leaves
  differing at once. A garbage or very stale digest draws the whole member list.
- A dropped `Part` is the failure M43 works to make rare: two views that differ stay
  different until a `Part` gets through. Past about 1,000 differing members, none ever will.
- **`Sync` has the same failure** (spec review). A node answers a `Sync` with its *whole*
  view whenever the member counts differ, whatever its size. A cold node with 32 or fewer
  members reconciles by `Sync`, so a node in a fleet past about 1,000 answers the ordinary
  join path with a datagram that is dropped.
- `swim.rs`'s comment on `MAX_DATAGRAM` still says a reconciliation carries the whole member
  set. That was true before M34.

## Delta

1. **`pstore_gossip::MAX_DATAGRAM = 65_507`**, public. It is the largest message this crate
   puts on the wire. `swim.rs` reads into a buffer of that size, so sender and reader cannot
   disagree. Its comment is corrected.
2. **A `Digest` or `TaggedDigest` answer is one or more `Part`s.** Members go into `Part`s in
   the order the filter yields them. A new `Part` starts when the next member would push the
   current one's encoded length past `MAX_DATAGRAM`.
   - The length is computed exactly: a 21-byte header (tag, sender, count) and each member's
     encoded length. It is not estimated, and not found by encoding and retrying (guidance:
     no criterion can tell the two apart).
   - A member that cannot fit even alone (an address or zone over about 65 KB) still gets a
     `Part` of its own. The socket drops it, as it does today. Splitting does not make such a
     member reachable, and silently leaving it out would hide that it exists.
3. **A `Sync` is answered with a `Sync` when the whole view fits one datagram, and with the
   whole view split into `Part`s (rule 2) when it does not.** ⚠️ The decision is the same
   split: the view is split by rule 2, and one chunk is sent as a `Sync` (whose header is
   also 21 bytes). There is no second length check to be off by one (spec review). The sender learns everything it
   would have learned from the `Sync`. A `Part` draws no reply, so the reply loop `Sync`'s
   count check guards against cannot start. A view that fits answers exactly as today, which
   covers every existing test fleet. `reconcile()` sends a `Sync` only at 32 members or fewer,
   so its own `Sync` always fits.
4. **Nothing else changes.** A `Part` is absorbed without a reply, so receiving two is
   receiving their union. The wire format, `Ping` and `Ack` (at most 7 members), and M43's
   thresholds are unchanged. An old node absorbs each `Part` as it always has.

**Not changed:** the total bytes of an answer (splitting adds 21 bytes per extra `Part`), or
how many members a garbage digest draws. Row 53 keeps that residue. A send that fails is still
uncounted in swim.rs; only a member over about 65 KB can still cause one after this.

**New, and said so (spec review):** an answer that used to fail at the socket now arrives. A
`Sync` of about 70 bytes from a forged source address draws about 260 KB from a 3,000-member
node, in about four datagrams. Gossip trusts its network today (`learn()` takes the reply
address from the UDP source), so this is residue for row 53, not a change of trust model.

## Acceptance criteria

1. **The boundary is exact.** Members are built so their sizes are known to the byte.
   - Two members whose `Part` encodes to exactly 65,507 bytes travel in one `Part`.
   - With one address one byte longer (65,508), they travel in two.
   - So an implementation that leaves out the 21-byte header, or compares with `<` rather
     than `<=`, fails. Test: `pstore-gossip` `protocol::tests::a_part_of_exactly_the_datagram_is_one_part`.
2. **A large answer is split, tightly, in order.** A view of 3,000 members, each with a
   40-byte address and an 8-byte zone, answers a `Digest` of all-zero bucket sums.
   - The answer is more than one `Part`, and every encoded message is at most `MAX_DATAGRAM`.
   - The `Part`s, concatenated, equal the members in the order the view yields them, so each
     appears exactly once.
   - Every `Part` but the last would exceed `MAX_DATAGRAM` if the next member were added. So
     the split cannot pass by sending one member per datagram.
   - Test: `protocol::tests::a_large_answer_is_split_under_the_datagram`.
3. **A `TaggedDigest` answer is split the same way.** The same 3,000-member view answering a
   `TaggedDigest` of all-zero sums and tags gets `Part`s that each fit and that concatenate to
   all 3,000 in order. Test: `protocol::tests::a_large_tagged_answer_is_split_under_the_datagram`.
4. **A large `Sync` answer is split, and a small one is not.**
   - The 3,000-member view answering a one-member `Sync` sends `Part`s, each fitting, that
     concatenate to its whole view.
   - Delivered to a node of 2 members, those `Part`s draw no reply, and its checksum then
     equals the big view's: the reply converges and cannot loop.
   - A view that fits answers a `Sync` with one `Sync`.
   - Test: `protocol::tests::a_sync_answer_that_cannot_fit_is_sent_as_parts`.
5. **A member that cannot fit is still sent, alone.** A view where one member's address is
   70,000 bytes, answering a `Digest`.
   - That member is in a `Part` holding only it.
   - Every other `Part` fits.
   - The `Part`s concatenate to the view in order, so no neighbour is duplicated or lost.
   - Test: `protocol::tests::an_oversize_member_is_sent_alone`.
6. **A small answer is unchanged.** Every existing gossip and node test passes unchanged.
   `./scripts/gates.sh`.
7. **The node reads what the protocol may send.** `swim.rs` reads into
   `pstore_gossip::MAX_DATAGRAM` and has no constant of its own.
   - `grep -n "pstore_gossip::MAX_DATAGRAM" crates/pstore-node/src/swim.rs` matches.
   - `grep -c "const MAX_DATAGRAM" crates/pstore-node/src/swim.rs` prints 0.
8. **Mutation:** the incremental sweep of the changed lines misses 0. `./scripts/mutants.sh`.
