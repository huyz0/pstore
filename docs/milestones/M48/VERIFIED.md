# M48 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `pstore-gossip`'s deterministic
protocol and `Sim`. Every figure is datagram bytes or a count, none a timing.

Command: `cargo test -p pstore-gossip`. Tests 1, 2 and 4 were added before the code and seen
red against it. Test 3 already held, because nothing ever fell back, so it is shown red by
hand mutation.

1. **An agreeing digest is answered.** `an_agreeing_tagged_digest_is_answered_empty`: one empty
   `Part`, to the sender. Red before the code.
2. **Silence falls back; tags return.** `a_peer_that_never_answers_tags_is_sent_a_digest`:
   - tagged, tagged, tagged, then `Digest`;
   - still `Digest` after a `Part`;
   - tagged again after a `TaggedDigest` from the peer.
   - Red before the code. Killed by hand: the fallback disabled.
3. **An answering peer never falls back.** `an_answering_peer_keeps_its_tags`: ten mismatches,
   each answered, send ten `TaggedDigest`s. Killed by hand: a `Part` not clearing the count.
4. **A mixed pair converges.** `a_mixed_version_pair_converges`:
   - a new node and an emulated pre-M43 node, 119 of 121 Dead in both views;
   - the new node holds the old node's newer record, and the checksums agree, within 6 ticks.
   - Red before the code. Killed by hand: the fallback disabled, so they never converge.
   - Code review confirmed no other path converges them: the record bypasses piggyback, and
     no `Sync` is sent above 32 members.
5. **Nothing else moves, measured.** `./scripts/gates.sh` was green by the pre-commit hook on
   M48.1. Every existing gossip test passes unchanged.
   - `a_tagged_digest_cuts_heavy_loss_at_200`: **1,043 B per node per round on the parent
     (`52dd922`) and 1,142 with this change**, against a bound of 1,300. Each was measured
     with a temporary print, which was removed.
   - ⚠️ **That is not a 9.5% regression of the protocol** (code review, which re-measured).
     - The empty `Part`s cost about 7 B per node per round: 13,854 of them, 291 KB.
     - False marks were 21 fleet-wide, drawing 52 `Digest`s: negligible.
     - The rest is the `Sim`'s every-Nth drop pattern: extra messages change which datagrams
       drop.
     - Against the parent at other loss rates: 1,869 vs 1,839 at 1 in 8; 1,432 vs 1,475 at 1
       in 9; 831 vs 861 at 1 in 11; 703 vs 835 at 1 in 12; 688 vs 673 at 1 in 13. Cheaper at
       three of five.
     - Fallback alone, with empty `Part`s off, measured 1,045.
   - Headroom under the 1,300 bound is now 12%. The parent already broke that bound at 1 in 8
     and 1 in 9.
6. **Mutation.** `cargo mutants --no-config --profile mutants --in-diff` over M48's source diff
   (`52dd922..`, staged): 19 mutants, 16 caught, 3 unviable, **0 missed**.

**Fidelity, said so:** the emulated old node runs this build. After three mismatches it marks
the new node itself, but it never receives a `TaggedDigest`, so it sends a `Digest` exactly as
an old build would.

**Residue for row 53:**
- Cost still grows slowly with fleet size: act when a fleet nears the size where it binds.
- A forged or garbage digest draws the member list, which M49 bounds by the
  owner's choice.
- A peer whose own view stays at 112 or below is never unmarked once falsely marked. It is
  correct, and costs M34's price.
