# M38 — A relay that times out suspects nobody, and a stale claim is not refuted twice

**Serves:** [BACKLOG](../BACKLOG.md) row 53, which [M34](../M34/VERIFIED.md) opened. Under heavy
loss, gossip still pays for suspicion churn: 9,662 B per node per round at 100 members and 10%
loss.

## What is true today

Measured while planning this milestone, with a probe that was not committed. The probe was
M33's real-hop simulator (4 waves) with a per-message-kind byte count, run for 400 rounds.

| Fleet, loss | B/node/round | `Part` share | Suspicions originated | Self-refutations |
|---|---|---|---|---|
| 100, 10% | 9,662 | 8,288 | 3,063 | 16,232 |
| 100, 2% | 796 | 336 | 175 | 845 |
| 50, 10% | 3,457 | 2,133 | 1,591 | 6,647 |

"Self-refutations" here and below is the sum of every node's own incarnation. That is a proxy:
one refutation raises it by `max(mine, theirs) + 1`, not by 1. ⚠️ The suspicion column was
corrected in spec review: the first draft's figures were measured with fix 2 applied.

**Two amplifiers, one on each side of a suspicion.**

1. **A relay that times out suspects its target.** A `PingReq` helper records its ping to the
   target in `pending` with the flag "the indirect round was already tried" set (`true`). So
   when that one 2-hop path fails, which at 10% loss is 19% of the time, the helper suspects
   the target outright, without the indirect round it requires of its own probes.
   - Predicted from loss alone: 19% direct failure × 4% for all three 4-hop indirect paths
     failing ≈ 0.77% of probes, about 150 suspicions in 400 rounds at 100 members.
   - Measured: 3,063, of which spec review counted 2,953 as a helper's. About 6,500 indirect
     rounds × 3 helpers × 19% ≈ 3,700, which matches.
   - ⚠️ **And the relay entry leaks** (spec review). `relaying` is cleared only by an ack, so
     every relayed probe that is never answered stays in it for the life of the node. At 10%
     loss that is about 19% of relays.
2. **A stale claim is refuted again.** `absorb` calls `refute_self_above` for **any** non-alive
   claim about this node, even one at a lower incarnation than it already holds. So every
   peer still carrying an old suspicion makes it raise its incarnation again. Each raise
   changes every checksum, and the reconciliation that follows ships more stale copies.
   Measured: 16,232 for 3,063 suspicions.
   - In SWIM, a claim below the current incarnation is already refuted: the node's higher
     record outranks it wherever the two meet.

**Each fixed alone, then both,** measured the same way (100 members, 10% loss):
- the relay fix alone: 1,030 B, with 160 suspicions and 759 refutations, still 4.7 each;
- both: **707 B**, with 148 suspicions and 145 refutations. That matches the prediction from
  loss alone.
- At 2% loss, both give 90 B, the converged steady state, with no suspicion at all. At 50
  members and 10%: 375 B.

## Delta

1. **A relayed probe that times out is dropped**, with its relay entry: no suspicion, and no
   indirect round of its own. The asker's own timeout and indirect round decide, as SWIM
   specifies.
2. **A claim about this node is refuted only at or above its own incarnation.** A lower one is
   already outranked, so the node does not raise its incarnation. ⚠️ **It re-gossips its
   current record instead** (`note_update` on itself; spec review, blocker).
   - Ignoring the claim outright made a loop. Peer P holds A `Dead`@1 while A is at 3, and A's
     refutation has left its piggyback. P probes A, and A's ack makes P's `Ack` branch send
     the stale record as evidence instead of reconciling.
   - A ignored it and acked again. Above 32 members A also sent a `Digest`, which taught A
     P's view and taught P nothing. So P pinged again, forever.
   - Spec review reproduced the loop: two lossless 40-member views were still looping at
     10,000 hops, where the parent settles in 7.
   - Re-gossiped, A's next ack carries `Alive`@3, which outranks P's claim.
3. **The simulator counts what a period's wave cap pushed to the next period** (spec review,
   major). A reply loop inside one period was invisible to every cost test, because the cap
   cut it short.

**Not changed:**
- probing, the indirect round, suspicion timeouts, and death;
- the piggyback, the checksum, the buckets, `Digest` and `Part`;
- the wire format.

## Acceptance criteria

1. **A relay's timeout suspects nobody.** A helper handles a `PingReq` for a target that never
   answers, and ticks past the probe timeout twice over. The target is still alive in the
   helper's view, and the relay entry is gone.
   - **Parent:** the helper suspects the target.
2. **A stale claim is not refuted.** A node at incarnation 3 receives `Suspect` and `Dead`
   claims about itself at incarnation 2. Its incarnation stays 3. A claim at 3 still raises
   it to 4.
   - **Parent:** 3 → 4 on the stale claim.
2b. **A stale claim ends in a few hops.** Two views of 40 members exchange only with each
   other, losslessly. P holds A `Dead` at incarnation 1, and A is at 3 with an empty
   piggyback. Within 20 hops, P holds A `Alive` at 3, the exchange stops, and A is still at 3.
   - **Parent:** settles, but A raises its incarnation.
   - **Under fix 2 as first drafted:** loops past 10,000 hops (spec review).
3. **Under loss the cost falls, and the bounds tighten.** On M33's real-hop model, in bytes per
   node per round:
   - 100 members at 10% loss: at most 1,000 (today 9,662, unbounded by any test);
   - 50 at 10%: at most 600 (today 3,457; M34's bound was 4,000);
   - 100 at 2%: at most 150 (today 796; M34's bound was 1,000).
   - M33's 20-member floor holds unchanged.
4. **Suspicion is what loss predicts.** At 100 members and 10% loss over 400 rounds, the sum of
   every node's own incarnation is at most 200 (300 in the spec as approved; tightened in code review, since a helper suspecting on its relay measured 295), and at most 15,000 replies are carried past a
   period's wave cap (spec review measured 11,775; the parent 31,260).
   - ⚠️ That counter does not detect a reply loop. Fix 2 without its re-gossip carried
     *fewer* (11,261). Criterion 2b is what catches the loop.
   - **Parent:** 16,232.
5. **Nothing is lost.** `a_lossy_network_does_not_manufacture_deaths` and
   `bucketed_reconciliation_manufactures_no_deaths` pass unchanged, and so does every
   convergence test.
6. **Gates.** `./scripts/gates.sh` is green, and the sweep over M38's source diff misses 0.

## Test plan

| # | Test | Red on the parent because / mutation it catches |
|---|---|---|
| 1 | `a_relay_that_times_out_suspects_nobody` (`protocol.rs` unit) | the helper suspects; the relay entry left behind |
| 1b | `a_relay_that_times_out_suspects_nobody`, its relay half | the `relaying.remove` on timeout deleted: the entry leaks |
| 2 | `a_stale_claim_is_not_refuted` (`protocol.rs` unit) | 3 → 4 on a claim at 2; `>=` as `>` (the claim at 3 not refuted); the re-gossip dropped |
| 2b | `a_stale_claim_ends_in_a_few_hops` (`tests/protocol.rs`) | the loop: no re-gossip on the ignore path |
| 3 | `gossip_cost_under_loss_on_both_models`, bounds tightened | today's bytes |
| 4 | `suspicion_is_what_loss_predicts` (`tests/protocol.rs`) | 16,232 for the incarnation sum, and 31,260 carried; ⚠️ not a loop detector (2b is) |

⚠️ **The bounds move in the tightening direction.** That is the fix landing, never the
weakening the rules forbid.

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Blob requests | 0 | 0 | 0 | 0 | none: gossip is datagrams |

## Risks

- **A helper no longer reports its own failed relay.** Its evidence was one 2-hop path, and the
  asker runs three, so the asker's timeout stays the authority. A target that is really down
  is still suspected by every node that probes it directly.
- **A stale claim is answered with the node's current record**, re-gossiped, not with a new
  incarnation. Each stale claim re-arms that record's retransmits, and the claims stop once
  the claimant has learnt it (criterion 2b).
- The nit to mark a relayed probe explicitly in `pending` is taken: a probe kind, not an
  inference from `relaying`.
- **The `Part` still grows at N/16 per differing bucket**, row 53's other half. At 10,000
  members one bucket is about 625 members. With churn at what loss predicts, fewer buckets
  differ at once. The slope stays, narrowed into the row.

## Tasks

- **M38.1** — Both fixes, tests 1–4.
- **M38.2** — The ledger, `BACKLOG.md` row 53 narrowed, and the roadmap row.
