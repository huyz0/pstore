# M23 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0). Every
count is a request count, exact on any machine; no number here is a latency.

Command: `cargo test -p pstore-engine --test segment_names`, unless a line names another.

**Observed red.**
- All 11 tests first written failed on the parent commit, `78e2845`.
- The four race tests reproduce row 44's loss: B's committed rows vanish (`{a}` for `{a, b}`), a
  merge loses `c`, and two delete vectors are replaced by the paused writer's.
- Tests added later, from code review or for criterion 10, guard a mutation instead. Each
  mutation named below was applied by hand and fails the test named with it.

1. **A paused fold never replaces a live segment.** `a_paused_fold_does_not_replace_a_live_segment`.
   Killed: the create as an unconditional `put`.
2. **A paused compaction never replaces a live segment.**
   `a_paused_compaction_does_not_replace_a_live_segment`, held at its re-seal after a lost CAS,
   as the spec's setup says. Killed: as 1.
3. **A paused fold never replaces a live delete vector.**
   `a_paused_fold_does_not_replace_a_live_delete_vector`. Killed: as 1.
4. **A branch's delete-vector copy never replaces a live one.**
   `a_branch_does_not_replace_a_live_delete_vector`, two sources sharing a segment with different
   vectors. Killed: as 1.
5. **A retry at its own epoch takes the next name.**
   - `a_fold_retried_at_its_own_epoch_takes_the_next_name`. Killed: a refusal answered as an
     error; the next name not carried across attempts.
   - `a_compaction_retried_at_its_own_epoch_does_not_reseal`. Killed: keys compared in place
     of epochs.
6. **An uncontended operation is unchanged on the wire.**
   `an_uncontended_fold_compaction_and_branch_are_unchanged` pins exact (reads, writes): a fold
   (11, 2), a fold that deletes (15, 2), a compaction (8, 2), a branch (2, 2). The same test,
   run against the parent commit's engine, printed the same four pairs.
   `a_refused_name_costs_exactly_one_write`: (11, 3). Killed: a probe (`head`) before each
   create. Both tests were added by code review round 1.
7. **The segment claims its name first.** `a_segment_claims_its_name_before_its_sidecars`.
   Red on the parent commit, which wrote A's centroid table at B's name.
8. **A refused name is never buried.** `a_refused_name_is_never_buried`, on a planted object.
   Killed: the refused name recorded.
9. **Bounded.** `sixteen_refusals_fail_the_operation`, exactly 16 refusals each for a fold and a
   compaction. Killed: the bound off by one, and exhaustion reported as `Lost`.
10. **Suffixed names are read like any other.** `a_suffixed_name_is_resolved_reaped_and_not_a_copy`
    (`as_of` serves a `_1` segment; GC reaps it with its sidecars).
    `a_suffixed_name_parses_as_its_base` and `a_suffixed_name_is_not_a_copy`, unit tests run by
    `cargo test -p pstore-engine --lib`. Killed: `-` in place of `_`.
11. **A fold buries what its discarded attempts created.**
    - `a_fold_buries_what_its_discarded_attempts_created`: after a lost CAS, two commits and no
      burial commit; every name is committed or buried, and GC reaps the buried ones.
    - `a_fold_left_with_nothing_buries_at_its_next_commit`.
    - Killed: no burial in the fold's commit; leftovers not carried; the burial's live filter off.
    - ⚠️ Amended at implementation, as the spec records: no burial commit of a fold's own.
12. **Gates.** `./scripts/gates.sh`: 17 of 17 by the pre-commit hook on `c50e7e0` and on this
    ledger's commit. The sweep over M23's source diff (`78e2845..c50e7e0`) misses **0** once its
    three misses were closed by tests.
    - pstore-engine: 48 mutants, 27 caught, 14 unviable, 6 timeouts, 1 missed. The timeouts are
      endless loops (a refusal count that never grows, a compaction that never ends), which
      cargo-mutants does not count as missed.
    - The miss was the fold's `retain` keeping what it had just buried. It is killed by
      `a_fold_left_with_nothing_buries_at_its_next_commit`'s new check that a reaped name is
      never buried again.
    - pstore-testkit: 8 mutants, 4 caught, 2 unviable, 2 missed: `Gated::only`'s narrowing, and
      `<` as `==` in its quota. Both are killed by `only_gates_the_named_keys` (`cargo test -p
      pstore-testkit --test injectors`). The second was also killed by tests that shard did
      not run.
    - ⚠️ Not `./scripts/mutants.sh` itself: `cargo mutants --in-diff` was run directly, with the
      engine shard limited to the eight test files covering the change, to fit the two-hour
      limit on this container.

**Existing tests, per the spec's treatment.**
- `abandon.rs`: `sealing_one_key_twice_writes_the_same_bytes` rewritten to "no key's bytes
  change". `an_error_after_sealing_buries_the_seal`'s fault moved to the create.
  `a_same_lane_winners_key_is_not_buried` and `a_key_already_buried_is_not_buried_twice` moved
  to an `Io` create that did not land.
  - Killed: the live filter off kills the first; the already-buried filter off kills the second.
- `retry_ceiling.rs`: HEAD only, with exact refusal counts of 24 (fold) and 48 (compaction).
  Killed: the fold's retry guard as `false`.
- `compaction.rs`: `Gated::only(n, "/HEAD")`. ⚠️ Drift: the spec said `Gated` would be narrowed.
  `Gated::new` is unchanged because the catalog's tests gate catalog keys with it.

Spec review took three rounds: round 1 blocked (one blocker, two majors), round 2 blocked (one
major), and round 3 passed. Code review took two: round 1 blocked (criterion 6's counts were not
pinned), and round 2 passed. Its five minors are in M23.1's commit, and m1 is BACKLOG row 49.
