# M49 — A digest cannot draw unbounded answers

> ⚠️ **Corrected by [M53](../M53/SPEC.md):** `MAX_DATAGRAM` is now 65,419, so `ANSWER_BUDGET` is
> 4 × 65,419 = 261,676 bytes, not 262,028. The exact-fill test sizes its members from the
> constant.

**Serves:** [BACKLOG](../BACKLOG.md) row 53's residue, "a garbage digest draws the member list",
which [M44](../M44/VERIFIED.md) made deliverable rather than dropped. The project owner chose
to bound it on 2026-10-04, before closing the row.

## What is true today

- A `Digest`, `TaggedDigest` or `Sync` is answered on arrival, every time.
  - A garbage `Digest` (all-zero sums) draws every member, split into `Part`s (M44): about
    260 KB from a 3,000-member view.
  - A `TaggedDigest` is now always answered (M48).
  - A `Sync` from a peer whose count differs draws the whole view.
- `receive` admits any sender as a member (`learn`), and answers go to the address it
  records. So a datagram claiming any id, from anywhere, draws a full answer, and N such
  datagrams in one tick draw N answers. Nothing bounds the bytes a node sends in answers.
- **Legitimate answers are few.** A node is probed about once a tick, and a checksum mismatch
  draws one reconciliation each way, so a node answers about one or two digests a tick.
  - The same peer can send two in one tick, one from its `Ping` path and one from its `Ack`
    path, when the two probe each other (spec review). Both carry the same state.
  - Measured on M33's `Sim` at 400 members and 10% loss (M43): 1,918 B per node per round
    in total.
- `pstore-node` ticks after each `DEFAULT_GOSSIP_PERIOD` (1 s; configurable, and it drifts), so
  a per-tick bound is about a per-second bound there.

## Delta

**A node answers at most one digest per peer per tick. Past the tick's first answer, it answers
only while the tick's total stays within `ANSWER_BUDGET` bytes. Digests past either limit are
dropped unanswered.**

1. **What counts.** The answers to a `Digest`, a `TaggedDigest`, or a `Sync` (its `Sync` or
   `Part`s), summed at their encoded length (M44's `member_len`, plus the 21-byte header).
2. **Per peer.** A second digest from the same peer in the same tick is dropped. The peer's
   next mismatch, a tick later, is answered.
   - Honest traffic does this when two peers probe each other in one tick (spec review). It
     is harmless: both digests carry the same state, and one `Part` clears M48's count
     outright.
3. **In all.** `ANSWER_BUDGET` is 4 × `MAX_DATAGRAM` (262,028 bytes) per tick.
   - **The tick's first answer is always sent whole**, whatever its size (spec review). A
     full-view answer is larger than the budget past about 3,200 members of 81 B. That is a
     cold joiner's `Sync` answer, an untagged peer's `Digest`, or a partition heal, and
     without this rule it would never be sent.
   - A later answer that would take the tick's total past the budget is dropped whole: no
     partial answer.
   - ⚠️ So a node's answers are bounded by `ANSWER_BUDGET` plus one answer per tick, however
     many digests arrive. Today they are bounded by nothing.
4. **Two counts of dropped digests**, `Protocol::budget_drops()` and `Protocol::repeat_drops()`.
   They make the claim that honest traffic never reaches the byte budget a test, not prose,
   and record how often the per-peer rule drops an honest repeat.
5. **Bounded state:** the per-peer record is a set of the peers answered this tick, cleared
   at the next `tick`.
6. **Not changed:** what an answer contains; `Ping`, `Ack` and `PingReq`; probing and
   suspicion; M48's fallback.
   - **Under a flood, M48 can falsely mark honest peers** (spec review). Drops are first come,
     first served, so a flood arriving first can starve honest peers' tags, and three
     unanswered tags mark them `untagged`.
   - A marked peer costs M34's price, bucket-level answers that use the budget faster, until
     it sends a `TaggedDigest`.
   - Bounded, and correct, but said so. Honest traffic never reaches the byte budget (criterion 5).

**Not addressed, said so:**
- What a flood can still draw: `ANSWER_BUDGET` plus one answer a tick, about 1.07 MB at 10,000
  members of 81 B (an 810 KB full-view answer plus the 262 KB budget), spent on answers to forged ids. It is a bound, not zero.
- **A forged `Ping` with a wrong checksum** draws an `Ack` plus a 401-byte `TaggedDigest`, or a
  `Sync` at 32 members or fewer, and this milestone does not count it (spec review).
- Gossip still trusts its network: answers go to an address any datagram can claim.
  Authenticated gossip is a larger change than this row asks for.

## Acceptance criteria

1. **One answer per peer per tick.** A node with a 121-member view receives two all-zero
   `Digest`s from the same peer in one tick. The first is answered and the second is not.
   A digest from a different peer in that same tick is answered. After a `tick`, a third from
   the first peer is answered. Test:
   `pstore-gossip` `protocol::tests::a_peer_draws_one_answer_a_tick`.
2. **The tick's budget counts bytes, not answers.** A node with a 1,000-member view (`crowd`:
   about 81 KB an answer) receives all-zero `Digest`s from 10 different peers in one tick. The peers are ids **already in the view**, so
   `learn` adds no member and the answer size is fixed.
   - It answers exactly 3: 3 answers fit in 262,028 bytes, and 4 would not.
   - After a `tick`, a new digest is answered.
   - A rule of "one answer a tick" answers 1, and an unlimited one answers 10, so both fail.
   - Test: `protocol::tests::a_tick_sends_at_most_its_answer_budget`.
3. **A `Sync` counts.** In that setup, after the 3 answers (about 243 KB spent), a `Sync` from
   an 11th peer, whose whole-view answer is about 81 KB, is not answered. Test:
   `protocol::tests::a_sync_answer_counts_against_the_budget`.
4. **A first answer larger than the budget is sent.** A node with a 3,500-member view (about
   284 KB an answer, past the budget):
   - answers the tick's first all-zero `Digest` whole, every member once;
   - drops a second from another peer in the same tick;
   - answers again after a `tick`.
   - Test: `protocol::tests::the_ticks_first_answer_is_sent_whatever_its_size`.
5. **Honest traffic never reaches the budget, measured.** `a_tagged_digest_cuts_heavy_loss_at_200`
   and the lossy-convergence tests gain the assertion that every node's `budget_drops()` is
   0. That strengthens them, and no bound moves. The ledger records the honest
   `repeat_drops()` count of the 200-member run. Every other existing gossip test passes unchanged:
   the "goes quiet" tests, M33's suspicion bounds, and the 200-member bound (≤ 1,300).
   `./scripts/gates.sh`. The ledger records that test's figure on the parent and with the
   change.
6. **Mutation:** the incremental sweep of the changed lines misses 0
   (`--no-config --profile mutants`). `./scripts/mutants.sh`.
