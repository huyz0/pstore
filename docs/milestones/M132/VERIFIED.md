# M132 verification

The spec was reviewed in three rounds, recorded in the [spec](SPEC.md). Criterion 5 was
amended during implementation; the amendment is marked in the spec.

How to run the tests:
- engine: `cargo test -p pstore-engine --test idempotency`;
- server: `cargo test -p pstore-server --test idempotency`;
- format units: `cargo test -p pstore-engine --lib keys`.

Red runs: every test was first run against the parent, where it did not compile (no
`IdempotencyKey`, `keyed`, `await_keyed` or `set_idempotency`). The mutations below were
applied one at a time by a script and then reverted. No `cargo mutants` run is claimed for
them.

1. `a_retry_is_answered_not_applied` (engine and server): green after. Killed by:
   - "rule 1 ignores listed records" (`x` became 1);
   - "the waiter sums its own attempt on a duplicate";
   - "rule 2 compares `>`";
   - (server) "the key is not stamped" and "`replayed` is never set".
2. `one_winner_in_one_fold` (engine): green after. Killed by "rule 1 ignores this fold's
   earlier attempt" (both `a` and `b` existed).
3. `at_most_once_under_contention` (engine, 100 attempts on 4 engines): green after. The
   run reported 5 commits for 100 attempts, so every replay had counts. Killed by "rule 1
   ignores listed records" and "the waiter sums its own attempt".
4. `a_retry_is_answered_not_applied` covers this criterion: its last retry is served by a
   new engine with empty caches.
5. `expiry_is_a_refusal_at_the_boundary` (engine) and `an_expired_retry_is_refused`
   (server): green after. Killed by:
   - "rule 2 off";
   - "a dropped entry does not raise `keys_since`";
   - "the waiter sums its own attempt".
6. `a_stale_key_never_applies` (engine; 32 commits apply, 33 are refused): green after.
   Killed by:
   - "rule 2 compares `>`" (the 32-commit key was refused);
   - "rule 2 off";
   - "only the first keyed fold initialises `keys_since`", with both initialisation sites
     mutated together.
7. `partial_is_reported` (engine): green after. Killed by "a keyed waiter answers
   `Unavailable` before reading its own records".
8. `refusals_and_levels` and `an_issued_key_is_free_while_the_mark_is_fresh` (server), and
   `a_key_ahead_of_head_is_refused` and `a_key_parses_only_as_issued` (engine): green after.
   Killed by:
   - "no `admit_key` at the door" (the `u64::MAX` key's row was written);
   - "no level check on the field";
   - "no level check on the issuing route".
9. `a_head_recommit_expires_never_readmits` (engine): green after. Killed by "a missing
   `keys_since` reads as 0".
10. `keys_round_trip_after_the_m130_bytes` and `a_stamp_carries_its_key` (`applied.rs`), and
    `keys_since_round_trips_after_the_m131_bytes` (`head.rs`): green after. "No keys
    section is written" was killed.
    - M131 read compatibility rests on two facts. M131's decode ended with
      `for … { h.outcomes.push(…) } Ok(h)` and no end check (see `git show 3ad35ee:crates/pstore-engine/src/head.rs`).
      And the test shows that the M131 bytes are a prefix.
    - The spec's plan named a test that decodes with a copy of M131's decoder. That test
      was not written; this argument replaces it.
11. `raising_is_not_the_flags` (engine): green after. Killed by "only a flagged engine
    raises `keys_since`".

Beyond the criteria:
- `only_a_flagged_engine_initialises_keys_since` (engine) checks that a compaction commit
  of a flagged engine initialises the field and an unflagged engine never does. Killed by
  "`commit_head` does not initialise".
- `a_vanished_outcome_is_read_again_not_ignored` (engine) checks that a listed object that
  answers "not found" during a fold makes the fold read HEAD again. Killed by "decide on the
  objects that were found".
- `a_refusal_whose_window_fell_is_partial` (server) checks that a refused attempt whose
  window fell off the list answers `idempotency_partial`. That happens here because another
  server committed 32 times inside this one's fresh mark. Killed by "partial reported as
  expired".
  - ⚠️ This is a real imprecision. An expired attempt on a busy tenant with several servers
    can be told "partial" when nothing of it applied. The docs say "was, or may have been".

Gates, in a cloud container: every workspace test ran under
`scripts/coverage.sh --all-features --no-fail-fast --fail-under-regions 95 --fail-under-lines 95`,
skipping M127's two `[::1]` tests (no IPv6 here): every test that ran passed; regions
95.05%, lines 96.60%, both floors held.
- A first measurement read 94.90% regions. The cause: `llvm-cov` counts a generic
  function's regions from its best-covered instantiation, so keyed branches inside the
  fold, the waiter and the write handler went uncounted. They moved into small functions
  (`decide_keys`, `list_outcome`, `initialised`, `caught_up`, `await_outcome`, `stamped`,
  the server's `admit`). No floor, test or threshold changed.
- Every mutation above was re-run after that move, by the same script, and killed.
- `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings` and
  `cargo deny check` passed.
- `scripts/check-poison.sh`, `check-links.sh`, `build-index.py --check`, `check-slos.py`,
  `check-milestone-ids.py` and `check-portable.sh` passed.
- `scripts/recall.sh`, `scripts/ndcg.sh` and `scripts/depth.sh` passed.
- NOT-RUN here: `scripts/mutants.sh` (`cargo-mutants` is not installed in this container).
