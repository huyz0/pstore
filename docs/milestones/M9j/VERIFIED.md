# M9j — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

1. **Restart after a fold** — `a_restart_after_a_fold_resumes_at_the_tail`
   (`cargo test -p pstore-engine --test restart`). **Observed red** on the unfixed engine: the
   flush returned `Seq(0)`. The rule "probe at `next` after a resume" was seen red at the
   `strong` assertion, with the `settled` guard removed by hand.
   - Criterion 1b is `a_warm_schema_cache_does_not_stand_in_for_the_resumes_head_read`. It
     was added for the sweep's one miss, which replaced `||` with `&&` in the flush's HEAD-read
     condition. The test was seen red with that mutant applied: it landed at `Seq(0)`.
   - Its first draft warmed the cache with a scan. That draft passed with the mutant applied,
     because `scan` does not fill the schema cache, so the draft uses a relevance query.
2. **Restart with nothing folded** — `a_restart_with_nothing_folded_keeps_every_bundle`.
   **Observed red**: `Seq(0)`.
3. **Cost** — the same test: the first flush costs 10 reads, 1 PUT and 0 LISTs; the second
   costs 0 reads and 1 PUT.
   - Two existing budget tests pinned the old first-flush cost and were amended as the spec's
     RA budget states, each with a comment giving the reason.
     - `the_whole_flow_stays_inside_its_request_budget`: 4 requests became 12. It gained a
       depth bound of at most 4 on the first flush and 1 after.
     - `a_durable_write_costs_its_lane_registration_once_then_one_put`: 2 reads became 10.
   - The steady-state assertions in both are unchanged.
4. **A failed resume** — `a_failed_resume_consumes_nothing_and_is_retried`. The store refuses
   one read inside E2's resume, and `failures()` is 1. **Observed red**: the unfixed flush
   succeeded.
5. **Through the server** — `a_restart_on_the_same_lane_keeps_every_acknowledged_write`
   (`cargo test -p pstore-server --test restart`). **Observed red**: `c` was lost.
6. **Gates**
   - `./scripts/mutants.sh --check . --in-diff <the M9j source diff>`: **12 tested in 19m, 11
     caught, 1 missed**. The miss is criterion 1b's.
   - After 1b's test: `--check 'replace \|\| with && in Engine<S>::flush_inner' --file
     crates/pstore-engine/src/lib.rs` gave **1 tested, 1 caught**.
   - Spec review: two rounds. Round 1 blocked on `strong` trusting the lane after a resume;
     round 2 passed.
   - Code review: one round, passed with no findings.
   - `./scripts/gates.sh` on this tree: all fifteen PASS.
