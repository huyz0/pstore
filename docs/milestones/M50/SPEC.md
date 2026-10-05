# M50 — A peer is sent one reconciliation a tick

**Serves:** the gossip residue the project owner asked to address on 2026-10-05: "fewer false
untagged marks". [M49](../M49/VERIFIED.md) recorded that its repeat drops doubled
[M48](../M48/VERIFIED.md)'s false marks.

## What is true today

- A node reconciles with a peer on every checksum mismatch: once from the `Ping` path (it was
  probed) and once from the `Ack` path (its probe was answered). When two nodes probe each
  other in one tick, each sends the other two digests.
- Since M49 the second is dropped unanswered as a repeat. Since M48 the sender counts every
  `TaggedDigest` it sends as unanswered until a `Part` arrives. So a dropped repeat can leave
  one on the count, and three unanswered mark a current peer `untagged`.
- **Measured** while planning, on the 200-member, 10%-loss `Sim` run that
  `a_tagged_digest_cuts_heavy_loss_at_200` makes, with temporary counters, not committed:
  - 74 `untagged` marks over the run, every one false, since every node reads tags;
  - 5,450 repeat drops;
  - 1,163 B per node per round.

## Delta

**A node sends a peer at most one reconciliation (`Sync`, `Digest` or `TaggedDigest`) per
tick.** A second mismatch with the same peer in that tick sends nothing. The pair reconciles at
its next mismatch: in the unit test, the next tick; in a fleet, the next time either probes
the other.
- ⚠️ **What it gives up** (spec review): when the first digest is lost, the second was a
  retry within the period, and the `Sim`'s four waves let it land. That is about 10% of the
  5,450 repeats: some 545 over 40,000 node-rounds. Every `Sim` liveness bound (`worst ==
  200`) would catch it if it mattered.

1. `Protocol` keeps the set of peers it reconciled with this tick, cleared by `tick`, as M49's
   answered set is. It is bounded by the view.
2. Both call sites, the `Ping` and `Ack` paths, consult it. M48's count, `reconcile`'s choice
   of message, and every answer are unchanged.
3. The `repeat_drops` doc comment keeps M49's history ("37 to 74") and adds this milestone's figures.
4. **A counter of marks**, `Protocol::untagged_marks()`: how many times a peer was newly marked
   `untagged`. So false marks are measured by a test, not by a temporary print.
5. **The prototype measured** (same run, then reverted):
   - **1 mark** (from 74);
   - **15 repeat drops** (from 5,450);
   - **1,116 B** per node per round (from 1,163).

   The second digest was redundant: the pair's state did not change within the tick. Sending
   it cost bytes and false marks.

**Not changed:** M48's fallback for a real old build, which still converges
(`a_mixed_version_pair_converges`); M49's answer limits; probing and suspicion.

## Acceptance criteria

1. **One reconciliation a tick per peer, across both paths.** A node viewing 121 members, in one
   tick:
   - receives a mismatched `Ping` from a peer and reconciles;
   - receives a mismatched `Ack` from the same peer and does **not** reconcile. That is the
     case that leaves the marks, and it shows one set serves both paths;
   - receives a second mismatched `Ping` from that peer, which is still `Ack`ed and not
     reconciled;
   - reconciles a mismatched `Ping` from another peer.

   After a `tick`, the first peer is reconciled again. Test: `pstore-gossip`
   `protocol::tests::a_peer_is_reconciled_once_a_tick`.
2. **False marks fall, measured.** `a_tagged_digest_cuts_heavy_loss_at_200` asserts that the
   fleet's total `untagged_marks()` is at most 5, where the parent measured 74. The ledger
   records both counts, the bytes and the remaining repeat drops, with what causes them.
   - The prototype measured 1. The bound is 5, not 1, because the `Sim`'s every-Nth drop
     pattern shifts counts whenever traffic changes (M48). That margin is still more than 14
     times under the parent's figure, which fails it. The `Sim` is deterministic, so the
     bound cannot flake.
3. **A real old build still falls back.** `a_mixed_version_pair_converges` and M48's unit tests
   pass. The mixed pair now reaches its `Digest` later, about tick 3 against tick 1, inside
   its 6. The ledger records the tick, and re-runs the fallback-disabled hand mutation against
   the new code. The unit tests modelled each mismatch as a round within one tick, so they gain a
   `tick` between rounds, and none of their assertions changes. The ledger names each such
   change.
4. **Nothing else moves.** `./scripts/gates.sh`, including every `Sim` bound and M49's
   `budget_drops() == 0` assertions.
5. **Mutation:** the incremental sweep of the changed lines misses 0
   (`--no-config --profile mutants`). `./scripts/mutants.sh`.
