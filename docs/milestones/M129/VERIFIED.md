# M129 verification

Spec reviewed in two rounds before code (round 2: one major, the window's lower bound,
fixed in the spec). Engine tests: `cargo test -p pstore-engine --test applied`; server
tests: `cargo test -p pstore-server --all-features --test applied_writes`. Red runs: the
ten engine tests then written ran against a stub `await_applied` (fold, then report
zeros); nine failed and `req_is_never_sealed` passed, because nothing was stamped yet.
Mutations below were applied by hand, one at a time, and reverted; no `cargo mutants`
run is claimed.

1. `applied_counts_are_what_the_fold_decided` (engine and server): red against the stub;
   green after. Mutation "evaluate a condition against the base, not the running state"
   killed (engine); "report the submitted counters" and "do not stamp" killed (server).
2. `exclusive_conditions_apply_once`: red against the stub; green after. 100 writers on
   4 engines behind a barrier, so the first fold judges many claims at once. The
   base-not-running-state mutation is killed here too.
3. `a_request_folded_elsewhere_sums_its_outcomes` and `a_cas_loser_reads_the_winners_outcome`:
   red against the stub; green after. Mutation "answer from this engine's cache only" killed.
4. `strong_reads_see_an_applied_write` (server): a durable write's control returns 503 on
   another server and the applied write's returns 200 with both rows. Mutation "respond
   without awaiting the commit" killed.
5. `req_is_never_sealed` (green on the stub, which stamped nothing; mutation "skip the
   strip" killed), `outcome_list_is_bounded_and_reaped` (red on the stub; "skip the burial"
   killed), `a_retired_outcome_is_unavailable` (red on the stub; "skip the fall-off check"
   killed). OBSERVED-NOT: dropping HEAD's list from GC's live set survives, because a
   listed key is never in the graveyard; it is defence in depth, not a tested behaviour.
6. `applied_needs_the_rollout_flag` (server): refusal with 0 read/write/list requests, and
   a durable response without the new fields. Mutation "ignore the flag" killed.
7. `concurrent_applied_writes_share_a_fold`: red against the stub; green after (≤2 HEAD
   commit attempts and ≤4 HEAD reads for 32 writers). Mutation "fold once per waiter, under
   the gate" killed by the read bound. `applied_cost_is_one_outcome_put_per_commit`: 3
   requests in one commit write 1 outcome object. `applied_write_cost_is_recorded`
   (`--nocapture`, MemoryStore, deterministic request counts): durable W 1, R 0, LIST 0,
   depth 1; applied W 4, R 14, LIST 0, sequential depth 10. No latency is claimed.
   Code review round 1 found two request rates that scaled with waiters; each got a test
   observed red first, then fixed: `a_burst_after_idle_reads_head_once_for_its_marks`
   (32 marks after an idle second read HEAD 32 times, now once) and
   `waiters_share_the_read_of_anothers_outcome` (32 waiters read another engine's outcome
   object 32 times, now once; the store delays outcome reads so unshared reads overlap).
   OBSERVED-NOT: "exactly 1 W more than the same commit without" is not asserted by a
   paired commit; stamped upserts also move an index onto the deferred path's reads.
8. `rejected_rows_are_counted` (red on the stub; "skip the reject-pass tally" killed) and
   `a_row_set_aside_after_resolve_is_rejected` (written after the first mutation run
   showed "skip the set-aside" surviving; it is killed now).

Review: round 1 minors not fixed, recorded in the commit body.

Gates, after rebasing onto M127, in a cloud container: `scripts/gates.sh` passed through
Clippy, then its `cargo test` failed only M127's two `[::1]` tests, which this container
cannot run (no IPv6; M127's ledger records the same). The workspace suite was then run as
`cargo test --workspace --all-features --no-fail-fast` skipping those two: 2,174 passed,
0 failed. Every later `gates.sh` step was run on its own and passed. `scripts/recall.sh`, `scripts/ndcg.sh`
and `scripts/depth.sh` passed on the rebased tree.
