# M58 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container. The server tests run real servers on
loopback over one in-memory store.

Commands:
- `cargo test -p pstore-engine --test split`: 8 tests.
- `cargo test -p pstore-server --test peers`: 22 tests.

⚠️ **The tests came first but could not compile until `SumCut` and `Part::sum` existed.** So
each was then **seen red under a hand mutation** of the code it checks, named on its line.

⚠️ **The spec had one review round, not two.** Its findings were applied directly: a cut per
share rather than per segment, the phase-2 fallback carrying the cut, and no split without a
text leg.

1. **Exact, in the engine.** `a_split_sum_query_equals_the_unsplit_one`: `query_split_as` under
   `sum` with weights 2 and 0.5 against `query_filtered_as`, hit for hit in score bits.
   - Both text legs have a limit of 1. `sum` runs its legs whole, so the limit must change
     nothing, and any share run leg by leg at its limit loses rows the answer has.
   - The fixture asserts that its best sum is neither leg's best alone, and that this row and
     at least one of the rows shadowed later lie in a peer's share (code review).
   - Cases:
     - plain;
     - filtered;
     - `top_k` past every match;
     - phase 2 failing;
     - the best three rows shadowed by unfolded writes;
     - shadowed with phase 2 failing.
   - Every case asserts a phased call per share, each carrying `keep = top_k + |shadow|`.
   - Killed by hand:
     - the peer cutting each leg at its limit (`whole` false);
     - the share's cut at `top_k`;
     - the fallback's cut at `top_k`;
     - the phase-2 fallback carrying no cut;
     - the cut not applied.
2. **Bounded replies.** In `a_split_sum_query_equals_the_unsplit_one`, no share sends back more than `keep` distinct rows,
   and in the cases where phase 2 succeeds at `top_k` 5, some share sends back at least one
   (code review: so the bound is measured, not vacuous). Killed by hand: the cut not applied.
2b. **No text leg, no split.** `split_tests::a_query_is_split_only_where_rule_seven_allows`:
   `sum` over dense legs gives no share, and `sum` over text gives two phased shares. Killed by
   hand: the text-leg guard removed.
3. **Through the API.** `a_split_query_answers_exactly_as_one_server_does` gains a weighted
   `sum` over two text legs, asked of each server. It is split (`pstore_peer_parts_sent`
   rises) and equals the unpeered server. `queries_that_cannot_be_split_run_on_one_server`
   now asserts that `sum` splits. Killed by hand: `shares` still refusing `Sum`.
4. **Protocol.** `a_sum_part_is_protocol_three`:
   - a peer refusing protocol 3 with `409` has its share run here, with an equal answer and
     one failure counted;
   - the part it was sent opened at protocol 3;
   - a real server refuses a cut by sum at protocol 2, and protocol 3 without one, with `400`,
     and accepts protocol 3 with one.
   - Added in code review:
     - a leg numbered 16 or more, or named twice, is refused; leg 15 is accepted;
     - a cut by sum in one exchange is refused;
     - a scan at another protocol than its part's is refused.
   - Killed by hand:
     - `sum` sent at protocol 2;
     - the explicit sum check removed;
     - the leg bound removed;
     - duplicates allowed;
     - the one-exchange refusal removed;
     - the scan protocol unchecked.
   - ⚠️ **Found in code review: a leg number sized the cut's allocation.** A caller naming
     leg 10¹² could have aborted the server. `guard` now refuses it, and `cut_sum` caps its
     width at `MAX_LEGS` besides. That cap is defensive behind the guard, so no test reaches
     it alone.
5. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - `cargo deny check`: no dependency was added. ⚠️ Advisories fail on `paste`
     (RUSTSEC-2024-0436) through `foyer`, as at M53 to M57: not this change.
   - **Mutation**, as M55 ran it (`--no-config --profile mutants`, `-j 1`):
     - `pstore-query/src/run.rs`, `fuse.rs` and `pstore-engine/src/lib.rs`, against the
       libraries' tests and the engine and query test files M55 named: **30 mutants, 5
       missed**, 20 caught, 5 unviable. That sweep ran before code review's fixes.
     - `pstore-server/src/peers.rs`, against the server's unit tests and `--test peers`, over
       the tree with code review's fixes: **35 mutants, 0 missed**, 28 caught, 7 unviable.
   - **The 5 misses:**
     - `Weights::all`, three return values, which only the server reads: pinned by
       `fuse::tests::weights_fill_to_one_and_refuse_past_the_bound`. Seen red with every
       weight 1.
     - `servers.len() < 2` as `<= 2`: two servers now split
       (`split_tests::a_query_is_split_only_where_rule_seven_allows`). Seen red.
     - `servers.len() < 2` as `== 2`: ⚠️ **equivalent, not tested**. With one server every
       segment is this server's by `assign`, and with none `assign` names none, so no share
       is built either way. `cargo fmt` reflowed the line, which is why it entered this diff.
   - ⚠️ The engine-side lines code review changed (the width cap in `cut_sum`, the `shares`
     doc) were not swept; the cap is defensive behind `guard`, whose refusal is tested.

**Not covered**, as the spec states: a peer's reply is up to `keep` rows × legs per share,
not `top_k` overall.
