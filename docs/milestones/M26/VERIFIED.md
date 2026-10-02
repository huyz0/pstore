# M26 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0). Time
is tokio's paused clock, so every interval here is exact; no number is a latency.

Commands: `cargo test -p pstore-engine --test idle` and `cargo test -p pstore-server --test
registry`.

**Observed red.**
- The 3 idle tests failed on an `is_idle` stub answering `true`.
- Each registry test fails with eviction disabled, except `unflushed_rows_are_never_evicted`
  and `eviction_costs_no_request`, which guard mutations instead.
- Every check named below was applied by hand and fails its test: each of `is_idle`'s six
  terms, and 17 server mutations.

1. **The registry is bounded.** `the_registry_is_bounded`: a cap of 4 leaves 4 of 20, and a cap
   of 20 leaves 18 of 30, with evictions counted. Killed: the target as `cap`.
2. **Unflushed rows are never evicted.** `unflushed_rows_are_never_evicted`: the tenant with the
   unflushed rows is the least recently used, and is kept with its rows visible. Killed:
   `pending` dropped (code review round 1).
3. **Nor is any other unrebuildable state.**
   - `each_kind_of_state_keeps_an_engine_busy` covers pending, durable, abandoned and uncertain.
   - `a_taken_lane_keeps_an_engine_busy` and `a_new_engine_and_a_settled_one_are_idle` cover the
     taken lane and the reapable term. Each term removed fails its own assertion.
   - `a_requested_or_backing_off_tenant_is_kept`: the requested tenant is the least recently
     used, and is kept. Killed: `owed` dropped.
   - ⚠️ Amended at implementation, as the spec records: a backing-off fold or reap is kept by
     its own state, so the backoff lookups were removed.
4. **Nor an engine in use.** `an_engine_in_use_is_not_evicted`. Killed: the `strong_count`
   check.
5. **Least recently used first.** `the_least_recently_used_goes_first`. Killed: most recent
   first, and last use not recorded.
6. **An evicted tenant comes back whole.** `an_evicted_tenant_comes_back_whole`: two tenants
   evicted, then every tenant reads, writes, flushes and folds.
7. **The cap is soft, and its scan backs off.**
   - `the_cap_is_soft_and_a_fruitless_scan_backs_off`.
   - `a_fruitless_scan_doubles_its_backoff`. Killed: `/ 2`, and the boundary `<=`.
   - `an_idle_engine_held_does_not_back_off`. Killed: `&&` as `||`.
   - `a_registry_at_its_cap_is_not_scanned`. Killed: `< cap`.
8. **Independent of the other duties.**
   - `eviction_runs_with_fold_and_gc_off`.
   - `a_served_registry_keeps_what_its_reap_loop_owes`: the real `serve_folding` with GC on
     keeps 3, and with fold and GC off keeps 1, read from `/metrics`. Killed: `set_reaping`
     made a no-op, and its call removed.
9. **No request is spent on it.** `eviction_costs_no_request`, a regression guard.
10. **Gates.**
    - `./scripts/gates.sh`: 17 of 17 by the pre-commit hook on `f6ee519`, and on this ledger's
      commit.
    - The sweep over M26's source diff (`3f71b29..f6ee519`) misses **0**:
      - pstore-engine: 13 mutants, 13 caught, against the idle tests;
      - pstore-server: 58 mutants, 42 caught, 16 unviable, against the registry, deploy,
        scheduled-fold and consistency test files.
    - ⚠️ Not `./scripts/mutants.sh` itself: `cargo mutants --in-diff` was run directly, limited
      to those test files, to fit the two-hour limit. `main.rs` is excluded, as every binary
      entry point is.

**Config.** `the_engine_cap_defaults_to_ten_thousand_and_refuses_a_non_count` (`cargo test -p
pstore-server --test deploy`).

Spec review took three rounds: round 1 blocked (a commit keeps a tenant busy for the reap age,
and the scan was on the request path), round 2 blocked (eviction rode optional loops), and
round 3 passed. Code review took two: round 1 blocked (the production wiring was untested), and
round 2 passed.
