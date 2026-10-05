# M52 — Leaves scaled to the fleet

**Serves:** the gossip residue the project owner asked to address on 2026-10-05: "scale with fleet
size". [M43](../M43/VERIFIED.md) fixed 256 leaves, and row 53 named "leaves scaled to the
fleet" as the move when a fleet reaches the size where this binds.

## What is true today

Measured while planning on [M51](../M51/VERIFIED.md)'s code (`b9a6821`): a dependency-free copy
of `pstore-gossip` in the scratchpad, release, the `Sim` at 4 waves, 10% loss (phase 0), 200
lossy rounds after 50 converged. Bytes per node per round, by message:

| Members | Total | Ping | Ack | Part | TaggedDigest | Members per Part | Untagged marks |
|---|---|---|---|---|---|---|---|
| 400 | 1,672 | 272 | 327 | 370 | 678 | 4 | 0 |
| 800 | 3,445 | 344 | 452 | 1,746 | 879 | 17 | 2 |
| 1,600 | 10,159 | 344 | 454 | 8,320 | 1,017 | 83 | 391 |

- **It binds at 1,600, through `Part`s.** Members per `Part` grow faster than the fleet, because
  two compounding things grow with it:
  - the leaves that differ between two views, from churn in flight;
  - the members per leaf, N/256.
- Above 800 the false marks of M48 return (391 at 1,600), because larger `Part`s mean more
  answers lost.
- **Prototypes, measured the same way and reverted:**
  - 1,024 leaves at 1,600 members: **about 6,122 B** (Part 2,405, tags 2,896, 22 members per
    Part, 64 marks).
  - 1,024 leaves at 400 members: 2,818 B, worse than 1,672, because tags dominate a small fleet.
  - 512 leaves at 800 members: **about 3,161 B** (Part 931, tags 1,411).
  - 1,024 leaves at 800: 4,012 B.
  - So the leaf count must scale with the fleet.
  - **4-bit tags** at 512 and 1,024 leaves were far worse (14,915 B of `Part` at 1,600): collisions
    fall back to whole buckets. Tags stay 8 bits.

## Delta

**A `TaggedDigest` carries 256 × 2^k leaf tags, where the sender picks the smallest k that keeps
its view at most 1.5625 members a leaf, capped at k = 7: k = 0 up to 400 members, 1 up to 800,
2 up to 1,600, 5 at 10,000, and 7 from 25,601 up.** The answerer reads the leaf count from the tags it receives.

1. **The leaf of a member at 2^k times the leaves** is `bucket × (16·2^k) + ((h >> 32) mod
   16·2^k)`. At k = 0 it is M43's leaf exactly, the prototype's formula at every k.
   - **Finer leaves nest by low bits, not by range** (spec review). The parent at k of a leaf
     c at k + j is `bucket(c) × 16·2^k + (c mod 16·2^k)`, where c is its index within the bucket.
   - So the children of within-bucket index p are p + m·16·2^k for m in 0..2^j, spaced
     16·2^k apart.
2. **`Cluster` keeps M43's 256 sums incrementally.** It computes sums at a higher k on demand, in
   one pass over the view: O(N), for a message sent about twice per node per round.
3. **The answer** compares the received tags with its own at the same leaf count.
   - The per-bucket mask widens from `u16` to 16·2^k bits (up to 2,048).
   - Sums above k = 0 are a computed `Vec`, so `leaf_tags`' "allocates nothing" holds only at
     k = 0, and its comment says so.
   - Otherwise M43's rules are unchanged: a bucket of 4 or fewer is sent whole, and so is a collision. It sends the
   members of the leaves that differ, in M44's `Part`s, under M49's budget.
4. **Wire:** tag 7 keeps its form, but its tag count is any 256 × 2^k for k = 0..=7, at most
   32,768 (a 32,913-byte message). Any other count is refused. A node from M43 to M51 refuses
   a count above 256, decoding it to nothing, so M48's fallback sends it `Digest`s once. After
   that, rule 5 keeps it on 256 tags.
5. **A peer that sends a 256-tag digest is sent 256 tags back** while our view is over 400,
   until it sends a finer one (spec review). Without this, a node from M43 to M51 in a fleet
   over 400 would refuse every finer digest. M48 would mark it, its own 256-tag digest would
   unmark it, and the cycle would repeat: three wasted ~1,169-byte digests per cycle at 1,600.
   - The record is a set of peer ids. An entry is removed when the peer sends a finer digest.
     Members are never removed from a view, so the set is bounded by the view.
   - A current peer whose view is at 400 or fewer also sends 256 tags, and gets 256 tags back:
     correct, at M43's price.
6. **Every fleet of 400 or fewer is unchanged**, byte for byte: k = 0. That includes every
   existing test, every bound, and M51's gate rows.

**Not changed:** buckets, `Digest`, `Sync`, M44's splitting, M48 to M50.

**Not measured, and not claimed:** 10,000 members. The `Sim` takes 238 s at 1,600, and 10,000
would take hours. The rule extends to it (k = 5, 8,192 leaves, an 8,337-byte digest), but no
number here says what it costs.

## Acceptance criteria

1. **The leaf count follows the view.** `reconcile` sends 256 tags at 400 members, 512 at 401 and
   at 800, and 1,024 at 801. Test: `pstore-gossip` `protocol::tests::the_leaf_count_follows_the_view`.
2. **Finer leaves nest and agree.**
   - The on-demand sums at k = 0 equal the incremental `leaves()`.
   - At k = 1 and 2, every member's fine leaf maps to its coarse leaf by the parent formula
     above.
   - Each coarse leaf's sum is the wrapping sum of the fine leaves that map to it.
   - Test: `cluster::tests::finer_leaves_nest`, or the crate's cluster test file.
3. **The wire accepts the family and nothing else.** Tag 7 round-trips at 256, 512, 1,024 and
   32,768 tags, and is refused at 0, 384, 65,536, and one byte truncated. Test:
   `wire::tests::a_tagged_digest_carries_any_power_of_two_leaves` (or the crate's wire tests).
4. **An answer at finer leaves is narrower.** Two views of 801 members, k = 2, differing in one
   member whose coarse leaf at k = 0 holds more members than its fine leaf at k = 2:
   - the answer's members are exactly that fine leaf's;
   - a 256-tag digest to the same view is answered by the coarse leaf, which is larger.
   - Test: `protocol::tests::a_finer_answer_sends_only_the_fine_leaf`.
4b. **An old node is sent what it reads.** A view of 801 receives a 256-tag `TaggedDigest` from a
   peer. Its next reconciliation to that peer carries 256 tags. After the peer sends a
   1,024-tag digest, it carries 1,024 again. Test:
   `protocol::tests::a_peer_that_sends_coarse_tags_is_sent_coarse_tags`.
5. **Cost at scale, measured.** A gate-scale `#[ignore]`d test, `the_cost_scales_with_the_fleet`,
   run by `scripts/gossip-loss.sh` beside M51's:
   - 800 members at 1/10, phase 0: at most **3,480 B**, the prototype's 3,161 plus 10%. ⚠️ This
     row is a **regression guard only**: the parent's 3,445 passes it too (spec review);
   - 1,600 members at 1/10, phase 0: at most **6,740 B**, the prototype's 6,122 plus 10%,
     against 10,159 on the parent, which fails the bound;
   - every view whole, no budget drops, and at most 100 marks at 1,600, where the parent had
     391.

   The ledger records each figure measured on the parent and on the change. The runtime, about
   283 s on this container and perhaps two to three times that on a CI runner, is
   `provisional`. The byte counts are deterministic.
6. **Nothing at 400 or fewer moves.** `./scripts/gates.sh`, and M51's
   `./scripts/gossip-loss.sh` rows, identical to M51's ledger.
7. **Mutation:** the incremental sweep of the changed lines misses 0
   (`--no-config --profile mutants`). `./scripts/mutants.sh`.
