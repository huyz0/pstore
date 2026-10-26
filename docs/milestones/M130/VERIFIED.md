# M130 verification

Spec reviewed once before code (four majors: cross-request order, ids versus counts, ids
across commits, mixed-version rollout; all addressed in the spec). Engine tests:
`cargo test -p pstore-engine --test applied` and `--lib outcome`; server tests:
`cargo test -p pstore-server --test applied_writes`. Red runs: the new tests ran against a
stub (`applying_with_ids` stamped as `applying` did, and `await_applied` returned no ids;
the server parsed `affected_ids` and ignored it). Mutations below were applied by hand by a
script, one at a time, and reverted; no `cargo mutants` run is claimed for them.

1. `affected_ids_are_what_the_fold_decided` (engine and server) and
   `a_set_aside_row_is_in_no_list` (engine): red against the stub (no ids); green after.
   Mutations "a set-aside keeps its id" and "count ids by request, not by id" killed.
   The server test's first expectation was wrong arithmetic (skipped 1 for 2 skipped
   operations: the absent patch and the absent delete); it was corrected to 2, not loosened.
2. `ids_union_across_commits_and_engines`: red against the stub; green after. Mutation
   "ids from the last commit only" first SURVIVED, because both commits upserted the ids
   asserted; the test now deletes a different id in each commit, and the mutation is killed.
3. `ids_are_opt_in` (engine: the commit's outcome object is M129's 88 bytes; server: no
   `ids` key) and `an_m129_outcome_object_decodes` (an M129 body spelled byte by byte
   decodes, and a commit without ids encodes to it exactly): both green on the stub, which
   wrote no ids section; mutation "always write the ids section" killed by both.
4. `affected_ids_refusals_cost_nothing` (server): red against the stub (the durable case
   was served); green after, with read/write/list counts unchanged for every refusal.
   Mutations "no by-filter refusal", "accept at level 1", "accept beside durable" and
   "refuse at exactly 10,000 distinct ids" killed.
5. `one_request_applies_in_the_documented_order` (engine, with and without a flush
   splitting the request into two bundles; server): engine red against the stub at its ids
   assertion, its counts already green (the order existed; M130 documents it); server red
   against the stub because the index had no rows to query, fixed in the test by seeding
   one. Mutation "buffer deletes before patches" (server) killed.
6. `ids_are_never_sealed`: green against the stub and after. It guards the `:ids` stamp;
   the strip is by key, so the mutation "strip only an exact id match" has no counterpart
   in this code and was not run.

Gates, in a cloud container: `cargo test --workspace --all-features --no-fail-fast`
skipping M127's two `[::1]` tests (no IPv6 here, as M129 records): 2,185 passed, 0 failed.
`cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` passed.
`scripts/check-poison.sh`, `check-links.sh`, `build-index.py --check`, `check-verified.py`,
`check-slos.py`, `check-milestone-ids.py`, the self-tests and `check-portable.sh` passed;
`scripts/recall.sh`, `scripts/ndcg.sh` and `scripts/depth.sh` passed. NOT-RUN here:
`scripts/mutants.sh` and `cargo deny check` (their tools are not installed in this
container).

Coverage, after the push: CI's coverage job had been red since M129 at 94.98% regions
(floor 95%); M129 was pushed without running it. Measured here with `cargo-llvm-cov`
installed, skipping the two IPv6 tests: 94.98% regions before, 95.01% after a follow-up
commit adding tests for the unexercised outcome paths (nothing flushed, a reaped object,
a failed read, the cache bound, cross-index set-aside, every truncation). Each new test
was checked by a hand mutation that it kills. `cargo deny` is red on main since M127 for
four zstd advisories, which is not this milestone's change.

Review: one round, verdict pass with two minors, recorded in the commit body. (The packet
is numbered round 2 because an earlier packet build was cut off by a full disk and
deleted unread.)
