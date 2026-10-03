# M38 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `pstore-gossip`'s lockstep `Sim` with
M33's real-hop model (4 waves). It is deterministic, and the bytes are datagram bytes.

Command: `cargo test -p pstore-gossip`.

⚠️ **A test that fails to compile is not counted as red.** Each criterion names a red on the
parent, or the hand mutation that was seen to fail it.

1. **A relay's timeout suspects nobody.** `a_relay_that_times_out_suspects_nobody`: a helper that
   ticks past the timeout twice over still holds the target alive, runs no indirect round, and
   leaks no relay entry.
   - Red on the parent: `Suspect`.
   - Killed: the relayed probe suspecting, and the relay entry left behind.
2. **A stale claim is not refuted.** `a_stale_claim_is_not_refuted`: claims at 2 leave
   incarnation 3, and the current record is re-gossiped. A claim at 3 raises it to 4.
   - Red on the parent: 5.
   - Killed: `>=` as `>`, the re-gossip dropped, and every claim refuted.
   - 2b: `a_stale_claim_ends_in_a_few_hops`. Two lossless 40-member views settle within 20
     hops (code review counted 8), with P holding A `Alive` at 3 and A still at 3.
     - Red on the parent: A at 5.
     - Killed: the re-gossip dropped, which loops (spec review: past 10,000 hops).
3. **Under loss the cost falls.** `gossip_cost_under_loss_on_both_models`, in bytes per node per
   round:
   - 100 members at 10% loss: 734 (bound 1,000; parent 9,662);
   - 50 at 10%: 326 (bound 600; parent 3,457, M34's bound 4,000);
   - 100 at 2%: 90 (bound 150; parent 796, M34's bound 1,000);
   - M33's 20-member floor still holds, at 90 B.
4. **Suspicion is what loss predicts.** `suspicion_is_what_loss_predicts`, at 100 members and 10%
   loss over 400 rounds:
   - the sum of own incarnations is 151, against a bound of 200 (the parent 16,232);
   - 11,775 replies are carried past the wave cap, against a bound of 15,000 (the parent
     31,260).
   - The bound was 300 in the spec. Code review tightened it, since a helper suspecting on
     its relay again measured 295. Killed: that mutant.
5. **Nothing is lost.** `a_lossy_network_does_not_manufacture_deaths`,
   `bucketed_reconciliation_manufactures_no_deaths` and every convergence test pass unchanged.
6. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on M38.1, and on this ledger's commit.
   - The sweep over M38's source diff (`90ddeb3..cc62423`; code review then added a comment only): 13 mutants, 9 caught, 4 unviable, **0 missed**. It was run
     with `--no-config --profile mutants`, as M34's was.
   - Hand mutations: 5, all killed.

**Left open, in BACKLOG row 53:** a `Part` still grows at N/16 per differing bucket, about 625
members at 10,000. With churn at what loss predicts, fewer buckets differ at once.
