# M50 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `pstore-gossip`'s deterministic
protocol and `Sim`. Every figure is datagram bytes or a count, none a timing.

1. **One reconciliation a tick per peer, across both paths.** `a_peer_is_reconciled_once_a_tick`:
   - a mismatched `Ping` reconciles;
   - the same peer's mismatched `Ack` that tick does not;
   - a second `Ping` is acked and not reconciled;
   - another peer is reconciled;
   - after a `tick`, the first peer is again.
   - Red before the code.
   - Code review found the guard short-circuits correctly: the slot is used only on a
     mismatch, and on the `Ack` path only when a reconciliation is sent.
2. **False marks fall, measured.** `a_tagged_digest_cuts_heavy_loss_at_200` asserts that the
   fleet's `untagged_marks()` is at most 5.
   - Measured on this code with a temporary print, which was removed: **1 mark**, **15 repeat
     drops**, **1,116 B** per node per round. The parent (`8375353`) measured 74 marks, 5,450
     repeat drops and 1,163 B.
   - Killed by hand: the per-tick rule removed, giving 74 marks and failing the assertion.
   - The 15 remaining repeats were **not attributed**: no counter separates their causes.
     Neither path below counts them in M50's per-tick set, so they are the candidates, unmeasured:
     - `Sync` answers (`receive`'s `Sync` arm);
     - digests from both nodes of a pair that each reconciled once and crossed in flight.
3. **A real old build still falls back.** `a_mixed_version_pair_converges` passes.
   - It now converges at **tick 3** (tick 1 before), inside its 6, as the spec predicted.
   - Re-run with the fallback disabled, it fails.
   - M48's two unit tests, `a_peer_that_never_answers_tags_is_sent_a_digest` and
     `an_answering_peer_keeps_its_tags`, gain a `p.tick(0)` before each round. Their
     assertions and sequences are unchanged, and code review judged the edits honest.
   - The first of them now also asserts `untagged_marks()` is 0 before the fourth silence and
     1 after it (sweep and code review).
4. **Nothing else moves.** `./scripts/gates.sh` was green by the pre-commit hook on this commit:
   every `Sim` bound, and M49's `budget_drops() == 0` assertions.
5. **Mutation.** `cargo mutants --no-config --profile mutants --in-diff` over M50's source diff.
   - First sweep: 15 mutants, 2 missed. Both were the marks counter, never asserted above 0.
   - With the counter pinned: 15 mutants, 10 caught, 5 unviable, **0 missed**.
   - An `if true` in place of the insert's result is equivalent, because that block runs only
     for a peer not yet marked (code review).

**Given up, said so:** a lost first digest no longer has a second within the same period to
retry it. That is about 545 of 40,000 node-rounds in the 200-member run. Every `Sim` liveness
bound still holds.
