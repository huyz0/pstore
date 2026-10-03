# M33 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `pstore-gossip`'s lockstep `Sim`. It is
deterministic: every Nth datagram is dropped, and the fleet is ordered. The bytes are
datagram bytes on the simulated wire. No production code changed.

Command: `cargo test -p pstore-gossip --test protocol`.

⚠️ **A test that fails to compile is not counted as red.** Each criterion names the mutation that
was seen to fail it.

1. **The default model is unchanged.** Every `pstore-gossip` test passes with `waves` defaulting
   to 1: 21 in `protocol`, including the two new ones, and every other suite.
   `a_real_network_completes_an_indirect_round` pins the default. Killed: a default of 4,
   which every older test also passes, so the pin was added for it.
2. **The real-network model completes an indirect round.** `a_real_network_completes_an_indirect_round`:
   with 4 waves a lost direct ping is no suspicion, and with 1 wave it is one. Killed: waves
   ignored.
   - Code review, minor: 2 waves already satisfy it, since the relayed ack lands before the
     timeout. Recorded, not tightened.
3. **The measurements are pinned.** `gossip_cost_under_loss_on_both_models`: 20 members at 2% loss
   with 4 waves agree in 395 of 400 rounds at 98 B/node/round (bounds ≥ 350 and ≤ 200).
   - The tripwires: 50 members at 10% loss with 4 waves cost 7,961 B (bound ≥ 7,000,
     tightened from 4,000 in code review), and 1 wave agrees in 4 rounds (bound ≤ 10).
   - Killed: waves ignored, and one wave too few (3 waves: 323 rounds and 218 B, measured in
     spec review).
   - Debug run time 3.5 s, 0.1 s for the other test.
4. **Row 50 closes, row 52 opens**, with the table, the throttles tried and the deaths they
   caused, the two designs left, and the tripwires' purpose:
   `grep -n '^| 52 \|^| ~~50~~' docs/milestones/BACKLOG.md` prints both rows.
5. **Gates.** `./scripts/gates.sh`: 17 of 17 by the pre-commit hook on `d0c1cbd` and on this
   ledger's commit. M33 changes no production source, so the sweep has nothing in its diff.
   The three hand mutations above were all killed.

**The measurement**, every cell reproduced independently by the spec reviewer. Columns are
bytes per node per round, and rounds agreeing of 400:

| Fleet, loss | 1 wave (the `Sim`) | 4 waves (real hops) |
|---|---|---|
| 20, 2% | 2,396, 4 | 98, 395 |
| 50, 10% | 8,953, 4 | 7,961, 5 |
| 100, 2% | 12,579, 4 | 5,533, 8 |
| any, 0% | 74, 400 | 74, 400 |

Spec review took two rounds.
- Round 1 blocked the first draft. It changed the production indirect-probe timeout, which
  only the `Sim` needed: a real node handles datagrams on arrival.
- Round 2 approved the rescope to measurement.

Code review took one round, which passed with three minors. Two were fixed (a looser tripwire,
an unstated test assumption), and one is recorded.
