# M43 — A tagged digest: reconcile only the leaves that differ

**Serves:** [BACKLOG](../BACKLOG.md) row 53's residue, which [M38](../M38/VERIFIED.md) narrowed:
"a `Part` still grows at N/16 per differing bucket … Measure first, on M33's real-hop model, at a
fleet size where it binds."

## What is true today

Measured while planning this milestone on M33's real-hop `Sim` (4 waves; 400 rounds after 50
converged), in bytes per node per round:

| Members | 10% loss | members per `Part` (max) | 2% loss |
|---|---|---|---|
| 100 | 734 | 9.1 (33) | 90 |
| 200 | 1,721 | 23.2 (113) | 93 |
| 400 | **6,457** | 74.8 (325) | 113 |

- Under heavy loss the cost grows faster than the fleet: churn in flight makes most of the 16
  buckets differ between any two members, so a `Part` carries most of the member list.
- At light loss it stays flat.

**Two designs were prototyped** while planning (not committed):
- **Two-level:** `Digest`, then the leaf sums of the buckets that differ, then a `Part` of the
  leaves that differ. 400 members cost 2,437. Its extra hop doubled the replies carried past the
  wave cap (24,186 against `suspicion_is_what_loss_predicts`'s bound of 15,000), so it fails
  that test, and the bound is not moved.
- **Tagged:** one hop, below.

## Delta

**Above 112 members, a checksum mismatch sends a `TaggedDigest` where it sent a `Digest`.**

1. **`Cluster` keeps 256 leaf sums** beside its 16 bucket sums, in the same `insert`, so every
   change keeps all three (checksum, buckets, leaves) right by construction.
   - A member's leaf is `bucket_of(id) × 16 + ((fnv(id) >> 32) mod 16)`: the bucket takes the
     hash's low bits and the leaf its high half.
   - A bucket's sum is its 16 leaves' wrapping sum.
2. **`TaggedDigest`, tag 7: the sender's id, 16 bucket sums, and 256 one-byte leaf tags, 401
   bytes, fixed.** A leaf's tag is the top byte of `leaf_sum × 0x9e3779b97f4a7c15`. The decoder
   refuses any other length.
3. **It is answered with one `Part`**, whatever the answerer's own view size: the answer
   depends on the message's type, never on the receiver's size. For each bucket whose sum
   differs, the `Part` carries:
   - every member, when the answerer's bucket holds 4 or fewer;
   - every member, when **no tag in it differs**: a tag collision. So a difference the 64-bit
     sums show is never left unanswered;
   - otherwise, the members of the leaves whose tags differ.
   - ⚠️ **In O(N)** (spec review): one pass over the members against a 256-bit mask of the
     leaves to send, never a list searched per member.
4. **The switch is at 112 members** (7 per bucket), set by measurement at 10% loss: at 100
   members tags cost more than they save (757 against 734), and at 128 they save (838 against
   926).
   - Thin, and said so: one deterministic schedule at one loss rate, with 112 interpolated
     between those two points. It is a cost knob.
   - At light loss the choice hardly matters: `TaggedDigest`s cost 9 bytes per node per round
     at 400 members and 2% loss.
   - At 33 to 112 members M34's `Digest` and `Part` are unchanged.
   - At 32 or fewer, the whole `Sync` is unchanged.
   - `Digest`, `Part` and `Sync` stay understood. An old node decodes tag 7 to nothing.
5. **The `Sim` counts `TaggedDigest` as reconciliation**, as M34 counted `Digest` and `Part`, so
   no "goes quiet" test can pass vacuously.

**Not changed:** probing, suspicion, the piggyback, refutation, M38's two fixes, and every
existing bound.

## Acceptance criteria

1. **Under heavy loss the cost falls, and nothing dies.** On the real-hop model, 200 members at
   10% loss over 200 rounds cost at most 1,300 bytes per node per round, and every view holds
   200, asserted on one run.
   - Spec review measured this run: 1,648 on the parent, 1,043 on the prototype.
   - The 400-round figures above are the probe's.
2. **Convergence holds on the new path.** A partition of two 80s heals, agrees, and goes quiet
   (5 rounds with no reconciliation). The test asserts `TaggedDigest`s were sent, so it
   exercises the tagged path rather than passing on M34's.
3. **A tag collision is answered, fully or partly.**
   - Two views whose differing bucket holds more than 4 members and whose tags are all equal:
     the `Part` carries the whole bucket.
   - A partial collision, one leaf's tags differing and a second leaf colliding, with the
     answerer holding the newer copy of the first leaf so the steps are deterministic: the
     first `Part` carries the first leaf. Once that is reconciled, the next exchange sends the whole
     bucket. That is the Risks section's liveness argument, tested.
4. **The leaves are right by construction.** After a scripted sequence of joins, suspicions,
   deaths and refutations, the leaves equal a from-scratch computation, and each bucket equals
   its leaves' sum. `leaf_of` for three fixed ids matches literals computed outside this code.
5. **The wire is exact.** `TaggedDigest` round-trips, is 401 bytes, and is refused truncated or
   with a trailing byte.
6. **Nothing at or below 112 moves.** Every existing test passes unchanged, including
   `whole_sync_up_to_32_members_and_a_digest_past_it`.
   - A 112-member mismatch still sends a `Digest`, and a 113-member one sends a
     `TaggedDigest`.
   - And the receiver's side (spec review): a view of 112 or fewer that receives a
     `TaggedDigest` answers it with a `Part`.
7. **Gates.** `./scripts/gates.sh` is green, and the sweep over M43's source diff misses 0.

## Test plan

| # | Test | Red on the parent because / mutation it catches |
|---|---|---|
| 1 | `a_tagged_digest_cuts_heavy_loss_at_200` (`tests/protocol.rs`) | 1,648 on the parent; tags ignored (the whole bucket always) |
| 2 | `tagged_reconciliation_converges` | ⚠️ a guard, green on the parent but for its count of `TaggedDigest`s. Red when a `TaggedDigest` is never answered (spec review: the partition never quiets, and a view falls to 196). It does not catch a dropped collision fallback: no simulation here collides |
| 3 | `a_tag_collision_sends_the_whole_bucket` (`protocol.rs` unit), both halves | the fallback dropped: the only test that catches it |
| 3b | `a_bucket_of_four_is_sent_whole_and_of_five_by_leaf` (`protocol.rs` unit; added in code review, which found the ≤4 rule untested) | the ≤4 rule deleted, or inverted |
| 4 | `leaves_match_a_from_scratch_sum` (`cluster.rs` unit) | a leaf not updated on replace; the leaf hash changed |
| 5 | `tagged_digest_round_trips` (`tests/wire.rs`) | a wrong tag or length |
| 6 | `a_digest_up_to_112_members_and_a_tagged_one_past_it` | the switch at the wrong size; a size check on the answer path that answers nothing at 112 or fewer |

⚠️ The tests at 200 members run 200 rounds, not 400, to keep the suite's time. The 400-member
figures live in this spec and the ledger, from the probe.

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Blob requests | 0 | 0 | 0 | 0 | none: gossip is datagrams |

Per mismatch above 112 members, 401 bytes each way, plus the members of the leaves that differ.

## Risks

- **A rolling upgrade past 112 members** has M34's gap, one level up. A new node's
  `TaggedDigest` is dropped by an old node, so the new node learns an old node's state only by
  piggyback.
  - The old node still learns the new node's: its `Digest` is answered with a `Part`.
  - Membership is an optimisation (`membership.md`).
  - Revealer: rounds agreeing, in a mixed fleet.
- **Still linear, at a smaller slope.** A `Part` carries about N/256 members per differing leaf,
  about 39 at 10,000 members, against 625 today. The digest is a constant 401 bytes.
- **Collisions** are deterministic for a pair of states. A second difference hidden behind one
  waits for the next exchange, when the bucket sums still differ (criterion 3, tested).
- **A `Part` is still uncapped** (spec review).
  - A member encodes to about 50 bytes, so one datagram holds about 1,260 members.
  - A whole-bucket fallback at 10,000 members is about 625, and heavy churn can still make
    most leaves differ.
  - A garbage `TaggedDigest` sent to a member's address draws its whole member list: the same
    amplification M34's `Digest` already has, not a new one.
  - Both stay in row 53's residue.

## Tasks

- **M43.1** — Leaves, `TaggedDigest`, the protocol, tests 1–6.
- **M43.2** — The ledger, row 53 narrowed or closed, and the roadmap row.
