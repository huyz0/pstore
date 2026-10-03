# M43 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `pstore-gossip`'s lockstep `Sim` with
M33's real-hop model (4 waves). It is deterministic, and the bytes are datagram bytes.

Command: `cargo test -p pstore-gossip`.

⚠️ **A test that fails to compile is not counted as red.** The new tests name new items, so
each criterion names the hand mutation seen to fail it.

1. **Under heavy loss the cost falls, and nothing dies.** `a_tagged_digest_cuts_heavy_loss_at_200`:
   200 members at 10% loss over 200 rounds cost 1,043 bytes per node per round (bound 1,300;
   the parent 1,648), and every view holds 200.
   - Killed: tags ignored, so every differing bucket is sent whole (2,008).
   - The probe, run outside the suite over 400 rounds, at 10% loss: 100 members 734 (unchanged), 200 members 1,083 (from 1,721), 400 members 1,918 (from 6,457), every view whole. At 2% loss 100 members cost 90.
2. **Convergence holds on the new path.** `tagged_reconciliation_converges`: two 80s heal, agree,
   and go quiet, with `TaggedDigest`s sent. Killed: a `TaggedDigest` never answered.
3. **A tag collision is answered, fully or partly.** `a_tag_collision_sends_the_whole_bucket`:
   a full collision sends the whole bucket. A partial one sends the differing leaf, then the
   whole bucket.
   - Killed: the fallback dropped. This is the only test that catches it.
   - `a_bucket_of_four_is_sent_whole_and_of_five_by_leaf`, added in code review, which found
     the 4-or-fewer rule untested. Killed: the rule deleted, and the rule inverted.
4. **The leaves are right by construction.** `leaves_match_a_from_scratch_sum`: the leaves equal a
   recount at every step, and each bucket equals the sum of its leaves. Ids 1, 2 and 200 fall in
   leaves 65, 121 and 216, computed with a Python FNV-1a checked against its published vectors.
   - Killed: a leaf not reduced on replace, and the leaf hash changed.
5. **The wire is exact.** `tagged_digest_round_trips`: 401 bytes, refused truncated or with a
   trailing byte. Killed: a wrong tag, and a wrong length.
6. **Nothing at or below 112 moves.** `a_digest_up_to_112_members_and_a_tagged_one_past_it`:
   - 112 members send a `Digest` and 113 a `TaggedDigest`;
   - a view of 112 answers a `TaggedDigest` with a `Part`;
   - every existing test passes unchanged.
   - Killed: the switch at 111 and at 113, and a size check on the answer path.
7. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on M43.1, on its fix, and on this
     ledger's commit.
   - The sweep over M43's source diff (`c828883..f5f2045`): 74 mutants, 68 caught, 5
     unviable, 1 missed, `|` as `^` in the leaf mask's fold. That mutant is equivalent over
     distinct bits, so the fold became a sum, and the sweep over that fix caught all 12 of
     its mutants: **0 missed**.
   - Both sweeps ran with `--no-config --profile mutants`, as M34's did.
   - Hand mutations: 12, all killed.

**Residue (BACKLOG row 53, narrowed):** still linear at a smaller slope (a `Part` of about N/256
members per differing leaf); a `Part` is uncapped, and a garbage `TaggedDigest` draws the member
list, as M34's `Digest` already did; a rolling upgrade past 112 members learns old nodes' state
by piggyback alone. An empty `Part` is sent when only leaves the answerer holds nothing in
differ, as M34's path already did (code review, nit).
