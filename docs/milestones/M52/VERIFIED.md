# M52 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `pstore-gossip`'s deterministic
protocol and `Sim`, the gate in release. Every figure is datagram bytes per node per round or
a count, none a timing; the run times are this container's and `provisional`.

Command: `cargo test -p pstore-gossip`, 40 unit tests and every integration test passing. Every
new test below was seen red before the code it checks:

- **AC1, AC3, AC4, AC4b's first version and AC8** failed against stubs that kept the parent's
  256 leaves behind the new names (`leaf_at`, `leaf_sums`, `leaf_tags_at` ignoring their
  leaf count) and the parent's `helpers`;
- **AC2** failed against the same stubs, at the fold of the fine sums;
- **the rewritten rule 5's tests** (code review) failed under a hand mutation each, listed
  under AC4b.

1. **The leaf count follows the view.** `protocol::tests::the_leaf_count_follows_the_view`:
   - 256 tags at 400 members, 512 at 401 and 800, and 1,024 at 801;
   - `leaves_per_bucket` is 16 at 0, 1,024 at 25,600, and 2,048 at 25,601, 51,200, 51,201
     and `usize::MAX`.
   - Killed by hand: the cap's `<` set to `<=` fails it at 51,201. Added in code review.
2. **Finer leaves nest and agree.** `finer_leaves_nest` in `tests/cluster.rs`, with a dead
   member in the view:
   - the sums at 16 leaves a bucket equal `leaves()`;
   - at 32 and 64, each member's fine leaf maps to its coarse leaf by the parent formula;
   - each coarse sum is the wrapping sum of its fine leaves.
3. **The wire accepts the family and nothing else.**
   `a_tagged_digest_carries_any_power_of_two_leaves` in `tests/wire.rs`:
   - tag 7 round-trips at 256, 512, 1,024 and 32,768 tags;
   - it is refused at 0, 384 and 65,536 tags, and with one byte cut off.
4. **An answer at finer leaves is narrower.**
   `protocol::tests::a_finer_answer_sends_only_the_fine_leaf`: two views of 801, differing in
   one member whose coarse leaf holds more of the view than its fine one. The 1,024-tag answer
   is exactly the fine leaf's members; the 256-tag answer to the same view is larger.
   - ⚠️ M43's `a_bucket_of_four_is_sent_whole_and_of_five_by_leaf` read the old `u16`
     per-bucket mask. It now reads the per-leaf `Vec<bool>`, with the same cases and the same
     expectations.
4b. **An old node is sent what it reads, and only an old node.**
   - `protocol::tests::a_peer_that_leaves_finer_tags_unanswered_is_stepped_down`: three
     unanswered 1,024-tag digests, with the peer's own 256-tag digests between them, and the
     4th carries 256 with 0 untagged marks. Sent 256, a 256-tag digest from it clears the
     count. A 1,024-tag digest brings 1,024 back.
   - `protocol::tests::a_step_down_expires`: stepped down at tick 1,000, it is sent 256 at
     1,063 and 1,024 at 1,064.
   - `protocol::tests::receiving_coarse_tags_never_steps_a_peer_down`: ten rounds of a peer
     sending 256 tags and answering ours, and nothing is recorded.
   - `protocol::tests::only_a_member_past_400_is_stepped_down`: a non-member falls to
     M48's `Digest` and is not recorded. At 400 members a 256-tag digest clears the count and
     M48's fallback follows, as before.
   - ⚠️ **Code review overturned the spec's first rule 5**, which stepped down any peer that
     sent 256 tags to a view past 400. Two current nodes whose views grew through 112 to 400
     then held each other at 256 for good, and unvalidated sender ids were recorded without
     bound. The rule and this criterion were amended, and the first version is noted in the
     spec.
   - Killed by hand, each failing the named test:
     - (a) the first rule's insert on receiving 256 restored fails the first and third;
     - (b) the membership check removed fails the fourth;
     - (c) the expiry removed fails the second;
     - (d) any 256-tag digest clearing the count fails the first.
5. **Cost at scale, measured.** `the_cost_scales_with_the_fleet`, run by
   `./scripts/gossip-loss.sh` beside M51's test. The script now refuses unless both pass. It
   passes in 10 min 34 s, `provisional`. 1/10 loss, phase 0, 200 lossy rounds after 50
   converged:

   | Members | Parent (`b9a6821`) | M52 | Bound |
   |---|---|---|---|
   | 800 | 3,445 B, 2 marks, 0 drops | **3,295 B, 0 marks, 0 drops** | 3,480 |
   | 1,600 | 10,159 B, 391 marks, **98,601 drops** | **5,885 B, 0 marks, 0 drops** | 6,740 |

   - Every view stayed whole on both.
   - The parent's figures come from the same test on a scratch copy of the parent with the
     assertions turned into prints.
   - ⚠️ The 800 row is a regression guard only: the parent passes its bound too.
   - The first run of the change, before rule 7, failed this criterion on budget drops: 42,726
     at 1,600, all on nodes 0, 256 and 512. That run found the relay hotspot (the spec's
     amendment).
6. **Nothing at 400 or fewer moves but the relays.** The same run of `gossip-loss.sh`: every
   M51 bound and both orderings hold, unmoved.

   | Members | Loss | M51 range | M52 range | Bound |
   |---|---|---|---|---|
   | 100 | 1/10 | 550–753 | 532–631 | 830 |
   | 200 | 1/7 | 2,139–2,245 | 2,253–2,357 | 2,470 |
   | 200 | 1/10 | 937–1,116 | 910–1,072 | 1,230 |
   | 200 | 1/13 | 389–629 | 497–560 | 700 |
   | 400 | 1/10 | 1,649–1,809 | 1,617–1,803 | 1,990 |

   - The 1/7 row's headroom falls from about 9% to 4.6%. The ordering still holds:
     2,253 > 1,072 and 910 > 560.
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
7. **Mutation.** `./scripts/mutants.sh`'s incremental sweep of the changed lines, run as
   `cargo mutants --in-diff` over the three source files (`--no-config --profile mutants`,
   `-j 2`): **99 mutants, 0 missed**, 92 caught, 4 unviable, 3 timeouts.
   - Earlier sweeps found 2 misses: the cap's `<`, and a `>` in the first rule 5, a line now
     gone. A later sweep found 1: `-` to `+` in the expiry, which survived because the test
     stepped down at tick 0. Each now has a test that kills it.
   - The 3 timeouts hang the test, so they are caught by timeout, not missed:
     - `+=` to `*=` in the leaf-count loop never leaves it;
     - `%` to `/` or `+` in `helpers` skips past the cycle without end.
8. **Relays are spread, and pinned.** `protocol::tests::relays_are_spread_across_the_view`:
   - the four exact pins from the model below;
   - 3 distinct helpers on every call, never `me` or the target;
   - at least 32 distinct members over ticks 1 to 64 (the model: 60; the parent: 3);
   - at least 8 distinct first helpers for 16 askers (the model: 14) and for 16 targets (15);
   - a view of `me`, the target and one other gives that one.
   - Red on the parent: "64 ticks asked only 3 members".
   - The independent model the pins came from, in Python. The spec reviewer's own model gave
     the same pins, and the Rust matched them on its first run:

   ```python
   M = (1 << 64) - 1
   def fnv(n):  # FNV-1a over the 16-byte id [n, 0, ..., 0]
       h = 0xcbf29ce484222325
       for x in [n] + [0] * 15:
           h ^= x; h = (h * 0x100000001b3) & M
       return h
   def mix(seed, tick):
       h = seed ^ ((tick * 0x9E3779B97F4A7C15) & M)
       h ^= h >> 33; h = (h * 0xff51afd7ed558ccd) & M; h ^= h >> 33
       return h
   def helpers(me, live, target, tick):
       l = [n for n in sorted(live + [me]) if n not in (me, target)]
       s = mix(fnv(me) ^ fnv(target), tick) % len(l)
       return ["n%d" % l[(s + i) % len(l)] for i in range(min(3, len(l)))]
   # helpers(0, list(range(1, 65)), 1, 1)     -> n38 n39 n40
   # helpers(0, list(range(1, 65)), 5, 2)     -> n3 n4 n6
   # helpers(0, list(range(1, 65)), 64, 3)    -> n11 n12 n13
   # helpers(0, list(range(1, 65)), 33, 1000) -> n20 n21 n22
   ```

**M49 corrected.** M49's "honest traffic never reaches the budget" was measured at 200 members
only. It carries a correction banner on its criterion 5, and the `ANSWER_BUDGET` and
`budget_drops` doc comments say where 0 drops is asserted.

**Residue (gossip):**
- 10,000 members is not measured. The rule extends to it (k = 5, 8,192 leaves, an 8,337-byte
  digest), but no number here says what it costs.
- A mixed fleet of builds from before M52 is covered by unit tests only, not by a `Sim` run.
- Gossip still trusts its network.
