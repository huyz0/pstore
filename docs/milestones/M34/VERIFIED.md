# M34 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `pstore-gossip`'s lockstep `Sim` with
M33's real-hop model (4 waves). It is deterministic. The bytes are datagram bytes.

Command: `cargo test -p pstore-gossip`.

⚠️ **A test that fails to compile is not counted as red.** Each criterion names a red on the
parent, or the hand mutation that was seen to fail it.

1. **Under loss the cost falls.** `gossip_cost_under_loss_on_both_models`, in bytes per node per
   round: 100 members at 2% loss pay 796 (bound ≤ 1,000; M33 measured 5,533), and 50 at 10%
   pay 3,457 (≤ 4,000; M33 7,961).
   - These were M33's tripwires, and moving them was the fix landing (BACKLOG row 52).
   - Killed: a whole `Sync` always (the parent's behaviour), a `Digest` answered with every
     member, and the switch lowered to 16 members.
2. **No deaths are manufactured.** `bucketed_reconciliation_manufactures_no_deaths`: 100 members
   at 10% loss, 400 rounds, every view at 100. `a_lossy_network_does_not_manufacture_deaths`
   passes unchanged. Killed: a `Part` never sent, and a `Part` not absorbed.
3. **Convergence holds above 32, and no floor moves.**
   - `reconciliation_above_32_converges_then_quiets` covers, each to agreement and then five
     quiet rounds:
     - a healed partition of two 40s, and one across the switch (two 20s);
     - a new member;
     - a two-sided difference;
     - zones over 40.
   - M33's floor (20 members at 2%: ≥ 350 agreeing, ≤ 200 B) holds: 395 rounds and 98 B.
   - `whole_sync_up_to_32_members_and_a_digest_past_it` pins the switch at its boundary. It
     was added for code review's finding that `>` as `>=` survived, and it kills that mutant.
   - `Sim` counts `Digest` and `Part` as reconciliation (spec review M3).
4. **The buckets are right by construction.** `buckets_match_a_from_scratch_sum`: after joins, a
   suspicion, a refutation, a death and a zone fill, the buckets equal a recount and sum to the
   checksum.
   - Ids 1, 2 and 200 fall in buckets 4, 7 and 13. The literals come from a Python FNV-1a,
     checked against its published vectors.
   - Killed: a bucket not reduced on replace, and the bucket hash changed.
5. **The wire is exact.** `digest_and_part_round_trip`: both round-trip and are refused truncated
   or with a trailing byte. A `Digest` is 145 bytes, and a `Part` is not decoded as a `Sync`.
   Killed: `Part` decoded as `Sync`, and a `Digest` written with a count.
6. **Gates.**
   - `./scripts/gates.sh`: 17 of 17 by the pre-commit hook on `a861209`, and on this ledger's
     commit.
   - The sweep over M34's source diff (`e138b03..a861209`): 31 mutants, 26 caught, 5
     unviable, **0 missed**.
   - ⚠️ Run with `--no-config` and `--profile mutants`. The repo's `.cargo/mutants.toml`
     passes `--features pstore-blob/object_store`, which a build of `pstore-gossip` alone
     refuses: the crate does not depend on `pstore-blob`. Its `test_workspace = true` would
     run the whole workspace per mutant. Not `./scripts/mutants.sh` itself.

**Residue (code review, minor, recorded):**
- removing `mark_alive` from the `Digest` or `Part` handler survives every test; the ping or
  ack before them already marked the sender alive;
- `encode` writes however many sums a `Digest` holds, though only `reconcile` builds one, from
  a `[u64; 16]`.

**Left open:** at 100 members and 10% loss the cost is still 9,662 B per node per round, and a
`Part` grows at N/16. That is BACKLOG row 53.

Spec review took three rounds.
- Round 1 blocked on a lowered agreement floor, a false mixed-version claim and a counter that
  made one test vacuous.
- Round 2 blocked on there being no convergence test above 32.

Code review took two rounds: the first blocked on the untested boundary.
