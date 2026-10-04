# M49 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `pstore-gossip`'s deterministic
protocol and `Sim`. Every figure is datagram bytes or a count, none a timing.

Command: `cargo test -p pstore-gossip`. The four unit tests for criteria 1 to 4 were added
before the code and seen red against it.

1. **One answer per peer per tick.** `a_peer_draws_one_answer_a_tick`:
   - the second digest from a peer is dropped, and counted as one repeat;
   - another peer is answered in the same tick;
   - the first peer is answered again after a `tick`.
   - Red before the code.
   - Added for the sweep, which found the counters unpinned: `repeat_drops()` is asserted 1.
   - Added in code review: `a_digest_that_draws_nothing_does_not_spend_its_peers_answer`. An
     agreeing digest draws nothing and does not use up the peer's answer. Killed by hand: the
     early return on an empty answer removed.
2. **The tick's budget counts bytes, not answers.** `a_tick_sends_at_most_its_answer_budget`:
   from a 1,000-member view (81,123 B an answer), exactly 3 of 10 digests are answered,
   `budget_drops()` is 7, and an answer goes again after a `tick`. Red before the code.
   - Added for the sweep, which found `>` versus `>=` unpinned:
     `answers_that_exactly_fill_the_budget_are_sent`. Two answers of exactly 131,014 B each,
     the budget exactly, are both sent, and a third is dropped.
3. **A `Sync` counts.** `a_sync_answer_counts_against_the_budget`: after 3 answers, a `Sync`
   whose whole-view answer would pass the budget is not answered. Red before the code.
4. **A first answer larger than the budget is sent.**
   `the_ticks_first_answer_is_sent_whatever_its_size`:
   - a 3,500-member view's whole answer, about 284 KB, goes, with every member once;
   - a second digest that tick is dropped;
   - one is answered after a `tick`.
   - Red before the code.
5. **Honest traffic never reaches the budget, measured.** The four `Sim` tests
   `a_lossy_network_does_not_manufacture_deaths`,
   `bucketed_reconciliation_manufactures_no_deaths`, `suspicion_is_what_loss_predicts` and
   `a_tagged_digest_cuts_heavy_loss_at_200` now assert that every node's `budget_drops()` is 0,
   and pass.
   - Killed by hand: `ANSWER_BUDGET` set to 100 bytes, and all four fail on that assertion.
   - The 200-member run, measured with a temporary print that was removed: **1,142 B per node
     per round on the parent (`d8fba72`), 1,163 with the change**, against a bound of 1,300.
   - Honest `repeat_drops()` over that run: 5,450, about 0.14 per node per round.
   - ⚠️ **Repeats raise M48's false marks** (code review, which re-measured): untagged marks
     over the run rose from 37 to 74. A dropped repeat can leave one on a peer's unanswered
     count. Bounded, and correct at M34's price; this likely explains the +21 B. The spec's
     "harmless" was too strong, and the code's comment on `repeat_drops` says this now.
   - ⚠️ **One existing test changed**, against the spec's "unchanged":
     `a_tag_collision_sends_the_whole_bucket` gains two `a.tick(0)` calls between digests from
     the same peer. Each exchange there is a round, and no assertion changed. Code review
     judged it an honest adaptation, not a weakening.
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
6. **Mutation.** `cargo mutants --no-config --profile mutants --in-diff` over M49's source diff
   (`d8fba72..`, staged).
   - First sweep: 35 mutants, 6 missed. Five were the counters, never asserted above 0. The
     sixth was the budget's exact boundary.
   - With the tests above, the sweep over the same diff: 35 mutants, 29 caught, 6 unviable,
     **0 missed**.

**Not addressed, as the spec says:**
- a flood can still draw `ANSWER_BUDGET` plus one answer a tick (about 1.07 MB at 10,000
  members);
- a forged `Ping` draws an `Ack` and a digest;
- gossip trusts its network.
