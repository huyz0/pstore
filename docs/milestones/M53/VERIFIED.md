# M53 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. The socket tests run real nodes over UDP on
loopback; nothing here is a timing or a measured throughput.

Command: `cargo test -p pstore-node -p pstore-gossip`, every test passing. The seal module and
its tests arrived together, so **every new test was seen red under a hand mutation** of the
code it checks, each named below. A mutation the tests did not catch is listed with the test
added for it.

1. **The seal holds.** `seal::tests::a_seal_opens_only_unaltered_under_its_key`:
   - the golden vector, computed with Python's `hmac` before any Rust ran, matches on the first
     run. The label is a literal in the test. The code reviewer recomputed it independently;
   - a sealed frame opens under its key and under a second accepted key, and is refused under
     another key, with any one of its 188 bytes flipped, and at every length short of the seal;
   - sealed for another `dest`, it opens but `admit` refuses it.
   - Killed by hand: verification skipped; the destination check removed.
2. **Freshness.** `seal::tests::a_stale_seal_is_refused`: 60 s either side opens, 60 s and 1 µs
   does not. Killed by hand: `<=` set to `<`.
   - **Before the receiver started** (added in code review):
     `seal::tests::a_frame_sealed_before_this_node_started_is_refused` (1 µs before refused, at
     and after opened), and over a socket, `a_frame_sealed_before_the_node_started_is_refused`.
   - Killed by hand: the check removed; `>=` set to `>`; the receiver passing a start of 0. The
     last survived every unit test, and is why the socket test exists.
3. **Replay.** `seal::tests::a_replay_is_refused_and_reordering_within_the_window_is_not`,
   `seal::tests::an_epoch_older_than_both_live_ones_evicts_nothing` and
   `seal::tests::an_evicted_epoch_stays_refused_when_its_sealer_holds_one`:
   - a repeat is refused; 63 below the highest is accepted once; 64 below is refused; a jump
     past the window clears it; two sealers never share a window;
   - a third epoch evicts the lower, whose datagrams and anything below are then refused, also
     after a sweep leaves the sealer one epoch; an epoch below both live ones evicts nothing.
   - Killed by hand: the window's `>=` set to `>`; the older-epoch refusal removed; the floor's
     `<=` set to `<`. The floor mutation survived the first two tests, and the third was added
     for it.
3b. **`dest` is chosen by rule 4.** `a_reply_is_sealed_for_its_verified_sender` in
   `tests/swim.rs`: a reply to a source is for its verified sealer; otherwise the member at the
   address, preferring one not declared dead, both dead the first; otherwise the derived id.
   Killed by hand: the reply branch removed; the dead-member preference removed; the fallback to
   a dead member removed.
4. **The table shrinks.** `seal::tests::a_quiet_sender_is_forgotten_after_the_window`: kept at
   the window's edge with its replay refused, not swept within a second, swept a second on, and
   a forgotten sealer starts afresh. Killed by hand: a sweep on every call; a sweep that keeps
   everything.
5. **The fleet**, real nodes over UDP on loopback, in `tests/swim.rs`:
   - `keyed_members_find_each_other`, with nothing refused;
   - `a_member_with_another_key_is_refused`: alone for 2 s, `refused()` above 0;
   - `keyed_and_unkeyed_members_refuse_each_other`, both ways. Killed by hand: a keyed node
     accepting an unsealed datagram that decodes;
   - `a_second_key_lets_a_rotation_roll`: pass 1 and pass 2 both meet. Killed by hand: only the
     first key opens;
   - `the_largest_sealed_part_is_received`: a sealed `Part` of exactly 65,419 bytes, 65,507
     sealed, sent from a raw socket, puts its member in the view. Killed by hand: the receive
     buffer back to `MAX_DATAGRAM`. ⚠️ Its first version waited for a member count of 2 and
     passed only because a learned member died (code review); it now waits for the member's
     address.
6. **Configuration refuses what it cannot honour.**
   `a_gossip_key_is_required_well_formed_and_swim_only` and
   `a_gossip_key_file_is_read_and_trimmed` in `tests/schedule.rs`: one and two keys, upper-case
   hex; refused at 31 bytes, odd length, non-hex, a `+` sign, three keys, a trailing comma, an
   empty key, a key with chitchat, no key without the opt-out, and the opt-out as `true`, `0`,
   `yes` or empty; the file trimmed, and an unreadable path refused. Killed by hand: any
   opt-out value accepted; the file untrimmed; a 31-byte key accepted; a key with chitchat
   accepted.
7. **The boundaries hold at the new size.** The two recalibrated tests,
   `protocol::tests::a_part_of_exactly_the_datagram_is_one_part` and
   `protocol::tests::answers_that_exactly_fill_the_budget_are_sent`, pass with their assertions
   unchanged. Each fails with its size moved one byte the wrong way.
8. **Nothing else moves beyond a bound.** `./scripts/gossip-loss.sh` passes, every figure
   identical to M52's ledger (800 members 3,295 B, 1,600 members 5,885 B, 0 marks, and all five
   M51 ranges unchanged). Its `Part`s never approach 65 KB, so the smaller datagram adds none.
   `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - `cargo deny check`: licences and bans pass with `ring` a direct dependency (it was already
     in `Cargo.lock`; the only lock change is pstore-node's dependency line). ⚠️ Advisories fail
     on `paste` (RUSTSEC-2024-0436, unmaintained), reached through `foyer`, which this change
     does not touch. Not fixed here.
9. **Mutation.** `./scripts/mutants.sh`'s incremental sweep of the changed lines, run as
   `cargo mutants --in-diff` over the five changed source files with both crates' tests
   (`--no-config --profile mutants`, `-j 1`): **127 mutants, 0 missed**, 115 caught, 12 unviable, in 55 min.
   - A first complete pass (stopped at 112 of 125 to fix what it found) missed 5, all in
     `Replay`. Three were the window's shift and mark (`-` to `+`, `<<` to `>>`, `|=` to `&=`),
     which no test exercised by replaying after the window moved. One was the emptiness check, never
     asserted true. One, `<` to `<=` on the older-than-both refusal, was equivalent where it
     stood, since an equal epoch never reached it; the refusal now runs before the lookup,
     where `<=` refuses a live epoch. Each has an assertion that kills it, seen by hand.
   - ⚠️ Two earlier attempts filled the disk: pstore-node's dependencies are large, and two
     jobs build two copies. `-j 1` fits.

**Not covered**, as the spec states:
- the UDP source address is not authenticated;
- any key holder is trusted;
- a node more than 60 s off cannot gossip, and one whose clock runs behind is refused by a
  freshly restarted peer for as long as it runs behind;
- datagrams are not confidential.
