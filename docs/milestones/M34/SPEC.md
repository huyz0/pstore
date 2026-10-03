# M34 — Reconcile only the buckets that differ

**Serves:** [BACKLOG](../BACKLOG.md) row 52, which [M33](../M33/VERIFIED.md) opened. Under loss,
gossip's cost grows with the fleet, because every exchange between disagreeing members ships the
whole member list.

## What is true today

- `pstore-gossip` members exchange an 8-byte checksum on every ping and ack: the wrapping sum
  of every member's fingerprint, kept incrementally (`Cluster::insert`). On a mismatch, either
  side sends a `Sync` carrying **every** member.
- Under loss, suspicion churn keeps checksums apart in nearly every round, so nearly every
  exchange pays O(N). M33 measured it in bytes per node per round, on its real-hop model (4
  waves): 50 members at 10% loss pay 7,961; 100 at 2% pay 5,533; 100 at 10% pay 15,225.
- Throttling `Sync` manufactures deaths. M33 measured three throttles: a `Sync` is what rescues
  a refutation whose piggybacked copies were lost.
- **Prototyped while planning this milestone, not committed:** 16 bucket checksums, a digest on
  mismatch, and only the differing buckets' members in reply. Bytes per node per round, 4
  waves:

| Fleet, loss | Today | 16 buckets | 64 | 256 |
|---|---|---|---|---|
| 20, 2% | 98 | 138 | 191 | 406 |
| 50, 2% | 1,786 | 385 | 596 | 1,643 |
| 50, 10% | 7,961 | 3,460 | 3,259 | 7,462 |
| 100, 2% | 5,533 | 797 | 917 | 2,387 |
| 100, 10% | 15,225 | 9,665 | 5,985 | 8,917 |

  - No deaths were manufactured at any point.
  - ⚠️ **Rounds agreeing fell**: 395 → 297 at 20 members and 2%, which fails M33's *floor*,
    not only its tripwire (spec review B1); and 54 → 28 at 50 members.
  - So the design below keeps the full `Sync` where it is cheap. A member uses `Digest` only
    when its fleet exceeds 32, twice the bucket count. Re-measured: 20 members at 2% is
    unchanged (98 B, 395 agreeing); 50 at 2% pays 386 B with 28 agreeing; 50 at 10% pays
    3,463 B; 100 at 2% pays 798 B; 100 at 10% pays 9,668 B. Every existing test passes but
    M33's tripwire, with `Sim` counting `Digest` and `Part` as reconciliation (spec review M3).

## Delta

**On a checksum mismatch in a fleet of more than 32, a member sends a `Digest` rather than a
`Sync`, and a `Digest` is answered with a `Part`: only the members in buckets that differ.** At
32 or fewer, a full `Sync` costs at most a few `Digest`s, and converges in fewer exchanges. It still reconciles on every
mismatch, but it reconciles less.
1. **`Cluster` keeps 16 bucket sums**, beside the checksum and maintained in the same
   `insert`, so every change keeps both correct by construction. A member's bucket is FNV-1a of
   its id, mod 16. The checksum equals the buckets' wrapping sum.
2. **Two wire messages:**
   - `Digest`, tag 5: the sender's id and exactly 16 bucket sums, with no count, 145 bytes;
     the decoder refuses any other length;
   - `Part`, tag 6: encoded as a `Sync`, and absorbed **without** a reply. A `Sync`'s reply
     rule ("answer if the member counts differ") would answer every `Part`.
3. **The protocol:**
   - a ping's or ack's checksum mismatch sends a `Digest` where it sent a `Sync`, past 32;
   - a `Digest` marks its sender alive and answers with a `Part`, when any bucket differs;
   - a `Part` absorbs and marks alive.
   - Each side of a mismatch sends its own `Digest`, so each learns the other's differing
     buckets.
4. **`Sync` stays understood**, and its handler is unchanged. An old node ignores the two new
   tags: an unknown tag decodes to `None`.

**Not changed:** the converged steady state (no mismatch, no message); probing; suspicion; the
piggyback; refutation.

## Acceptance criteria

1. **Under loss the cost falls.** On M33's real-hop model, in bytes per node per round: 100
   members at 2% at most 1,000 (today 5,533), and 50 at 10% at most 4,000 (today 7,961).
2. **No deaths are manufactured.** 100 members at 10% loss, 4 waves, 400 rounds: every view
   holds 100. `a_lossy_network_does_not_manufacture_deaths` passes unchanged.
3. **Convergence holds above 32, and no floor moves.** Every existing test passes, with `Sim`
   counting `Digest` and `Part`. M33's floor holds unchanged; only its tripwires move. On the
   new path each converges and then goes quiet (5 rounds with no reconciliation): a partition
   healed at 80 (halves of 40) and across the switch (halves of 20); a new member known to
   one of 40; a two-sided difference at 40; zones over 40 (spec review, round 2).
4. **The buckets are right by construction.** After a scripted sequence of joins, upserts,
   suspicions, deaths and refutations, the buckets equal a from-scratch computation and sum to
   the checksum. FNV-1a's bucket for three fixed ids matches literals computed outside this
   code.
5. **The wire is exact.** `Digest` and `Part` round-trip, and are refused truncated or with
   trailing bytes; a `Digest` is 145 bytes.
6. **Gates.** `./scripts/gates.sh` is green, and the sweep over M34's source diff misses 0.

## Test plan

| # | Test | Red on the parent because / mutation it catches |
|---|---|---|
| 1, 3 | `gossip_cost_under_loss_on_both_models`, updated: M33's tripwires become M34's bounds | today's bytes; a `Digest` replying with every member |
| 2 | `bucketed_reconciliation_manufactures_no_deaths` | a `Part` never sent (refutations lost); red by that mutation, since the parent has no `Digest` (spec review m6) |
| 3 | `Sim` counts `Digest` and `Part` | without it, `once_zones_are_known_no_sync_is_sent` would pass vacuously over endless `Digest`s |
| 4 | `buckets_match_a_from_scratch_sum` (`cluster.rs` unit) | a bucket not updated on replace |
| 5 | `digest_and_part_round_trip` (`tests/wire.rs`) | a wrong tag; `Part` decoded as `Sync` |

⚠️ **M33's tripwires move on purpose, which is the fix landing.** Row 52 says so. Their red on the
parent is today's bytes.

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Blob requests | 0 | 0 | 0 | 0 | none: gossip is datagrams |

Per mismatch, a `Digest` of 145 bytes each way, plus the members of the differing buckets.

## Risks

- **Slower agreement under loss, unpinned.** A `Part` reconciles part of the state per exchange.
  Bytes fall, and rounds agreeing fall too: 54 → 28 at 50 members and 2%. No criterion pins it;
  criterion 3 pins convergence once loss stops.
- **The rest still grows with the fleet, at a sixteenth of the slope** (spec review m5). A
  `Part` carries about N/16 members per differing bucket: at 10,000 nodes, ~625 members, about
  37 KB in one datagram. That is the new row's too.
- **Heavy loss is still roughly linear.** At 100 members and 10% loss it is 9,665 B per node per
  round. The rest is suspicion churn itself (Lifeguard's territory, OQ-12), and a new BACKLOG
  row records it.
- ⚠️ **A rolling upgrade is not reconciled both ways** (spec review M2). A new node in a fleet
  of more than 32 never starts a `Sync`, and answers one only when member counts differ. So an
  old node learns a new node's refutations by piggyback alone: the throttle that M33 measured
  manufacturing deaths under heavy loss. A reply rule that answered every differing `Sync`
  was tried. It sent two `Sync`s after convergence and failed
  `once_zones_are_known_no_sync_is_sent`, so it is not in this milestone. Membership is an
  optimisation: a wrong view costs cache hits, never correctness ([membership.md](../../research/04-cluster/membership.md)). So the upgrade is
  supported, and its transient cost is stated rather than hidden.

## Tasks

- **M34.1** — Buckets, `Digest`, `Part`, the protocol, and tests 1–5.
- **M34.2** — The ledger, row 52 closed and its residue's row opened, and the roadmap row.
