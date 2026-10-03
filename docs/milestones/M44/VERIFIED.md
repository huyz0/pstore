# M44 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. Every figure is an encoded message
length, computed by the test; none is a timing, so none is provisional.

Command: `cargo test -p pstore-gossip`.

All five tests were added before the code and seen red against `MAX_DATAGRAM` alone (the
constant, no split): each failed on a message over 65,507 bytes or on one `Part`.

1. **The boundary is exact.** `a_part_of_exactly_the_datagram_is_one_part`: a `Part` of exactly
   65,507 bytes is one `Part`; at 65,508 it is two. The sweep killed `>` as `>=` (extra 0) and
   the header dropped (extra 1).
2. **A large answer is split, tightly, in order.** `a_large_answer_is_split_under_the_datagram`:
   3,000 members of 81 bytes, every `Part` at most 65,507, concatenated equal to the view, every
   `Part` but the last full.
3. **A `TaggedDigest` answer is split the same way.**
   `a_large_tagged_answer_is_split_under_the_datagram`, with every tag complemented so every leaf
   differs (the spec's wording corrected in code review).
4. **A large `Sync` answer is split, and a small one is not.**
   `a_sync_answer_that_cannot_fit_is_sent_as_parts`: the 3,000-member view answers a one-member
   `Sync` with `Part`s; a 2-member node absorbs them with no reply and then has the same
   checksum; a view of 3 answers with one `Sync` of 3. The sweep killed `== 1` as `!= 1`.
5. **A member that cannot fit is still sent, alone.** `an_oversize_member_is_sent_alone`: a
   70,000-byte address travels as `[0,1,2] [3] [4,5]`.
   - Added in code review, which found the "always one chunk" rule untested:
     `a_differing_bucket_this_view_holds_nothing_of_is_one_empty_part`. Killed by hand: an empty
     last chunk skipped.
6. **A small answer is unchanged.** `./scripts/gates.sh`: green by the pre-commit hook on M44.1,
   with every existing gossip and node test passing unchanged.
7. **The node reads what the protocol may send.**
   `grep -n "pstore_gossip::MAX_DATAGRAM" crates/pstore-node/src/swim.rs` matches line 172, and
   `grep -c "const MAX_DATAGRAM" crates/pstore-node/src/swim.rs` prints 0.
8. **Mutation.** `cargo mutants --no-config --profile mutants --in-diff` over M44's source diff
   (`a9785da..820af0b`): 25 mutants, 20 caught, 5 unviable, **0 missed**.

**Residue (BACKLOG row 53, narrowed again):** still linear at a smaller slope; a garbage digest
draws the member list, now delivered rather than dropped, so a forged-source `Sync` of about 70
bytes draws about 260 KB from a 3,000-member node (gossip trusts its network today); a member
whose address alone exceeds a datagram still fails at the socket, uncounted; a rolling upgrade
past 112 members learns old nodes' state by piggyback alone.
