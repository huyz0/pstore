# M17 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

The engine tests are in `cargo test -p pstore-engine --test lane_taken`.

**Observed red:** eight of the first nine engine tests, and the server test, failed on the tree
before M17. They were run against the new `LaneTaken` variant and the flush hook, with the flush
unchanged. The ninth, `a_fold_inside_a_flush_is_no_second_writer`, passes on that tree. It is
pinned by mutation: the lane check with `>=` fails it. A check made where HEAD is read, the
mutation its spec names, was not run by hand. Every test
added later was seen failing first, as noted below.

1. **Loud, and erasing nothing.** `a_second_writer_on_a_lane_fails_loudly_and_erases_nothing`
   covers both of A's refusals, A's rows still visible to A, B's bundle unchanged, and a fold
   serving both.
   - It failed with the PUT unconditional, and with `Lost` read as this engine's own.
   - `a_lane_found_taken_stays_taken_after_the_collision_is_reaped` came from code review
     round 1: after the other writer's bundle is reaped, A still refuses, with no PUT. It failed
     before `taken()` became sticky.
2. **HEAD read after GC** — `a_head_read_finds_a_lane_taken_even_after_gc`. It failed with the
   lane check removed, and with the watermark not recorded.
3. **A lost acknowledgement** — `a_lost_acknowledgement_costs_one_get_and_nothing_else`: 1 PUT
   and 1 GET, then sequence 2, one row each, and a new process resumes past it. It failed when
   a landed write was reported as `Io`.
4. **Resolved first** — `an_unresolved_write_is_resolved_first_and_a_drop_takes_its_rows`. It
   failed with the drop leaving its count in the record.
   - `the_first_of_two_timed_out_writes_landing_late_is_its_own` came from code review round 1,
     which found the record kept only the last attempt.
   - `a_late_write_folded_before_its_retry_is_its_own` also came from round 1: a record found
     absent is resolved again once HEAD passes it.
5. **Written again** — `a_write_that_never_landed_is_written_again_at_its_sequence`. It failed
   with an absent bundle taken as landed, and with the watermark compared `>=`, which it now
   meets exactly.
   - `a_write_that_lands_late_is_its_own_not_a_second_writer` covers spec review N3.
   - `another_writer_where_a_write_was_found_absent_is_lane_taken` failed when a late landing
     accepted any bytes.
   - `a_write_landing_during_its_own_resolution_is_its_own` came from the sweep. It failed with
     `LaneTaken` because the flush kept the record as it was before its own resolution. That was
     a real race, now fixed.
6. **Absent past the watermark** —
   `an_unresolved_write_folded_and_reaped_is_lane_taken_not_a_guess`, and
   `another_writers_bundle_where_a_write_is_unresolved_is_lane_taken`. The second failed when the
   resolution accepted any bytes.
7. **`Contended`** — `contention_consumes_no_sequence`, which failed with `Contended` read as
   `Lost`. The API's 409 is `a_contended_commit_is_a_409_and_not_a_500`
   (`cargo test -p pstore-server --test errors`), whose `Contended` now arrives through the
   bundle write.
8. **The API** — `a_write_on_a_taken_lane_answers_lane_taken`
   (`cargo test -p pstore-server --test lane_taken`). It failed before M17: A's write answered
   200.
9. **No false alarm** — `a_fold_inside_a_flush_is_no_second_writer`, as above.
10. **Cost.**
    - `a_flush_that_creates_its_bundle_reads_nothing`: 1 PUT and no read; a collision, 1 PUT
      and no read. It failed with `Lost` read as this engine's own.
    - `an_absent_write_at_the_watermark_costs_one_put` came from the sweep. It failed with the
      re-resolution compared `>=`.
11. **Gates.**
    - `./scripts/mutants.sh --check . --in-diff` over M17's source diff `6b5d572..dafd60f`, in
      a worktree at `dafd60f`, in two shards: 42 mutants, 27 caught, 13 unviable, 2 missed.
      - `seen > seq` as `>=` is killed by the cost test above.
      - `r.absent && r.seq == seq` as `||` exposed the race in 5. After the fix the test is
        always true (code review round 3 agreed), so it became the record's presence, with the
        invariant asserted in debug builds.
    - The code changed after that sweep is swept with M18 and M19; their ledgers record it.
      Before that, the fifteen mutations in M17's test plan were run by hand, and each fails a
      test. One more, clearing the record after a successful write, is equivalent: a record left
      behind names a sequence this process has already written.
    - The combined sweep: `./scripts/mutants.sh --check . --in-diff` over the source diff `dafd60f..f52bf9a` (M17's remainder, M18 and M19), in a worktree at `f52bf9a`, in four shards: 83 mutants, 54 caught, 10 unviable, 19 missed. None of the misses is in M17's code; M18's and M19's
      ledgers account for them.
    - `./scripts/gates.sh` passed, all 17 gates, at `1e580e4`: the source swept, plus the tests that kill the misses.

Spec review took two rounds. Code review took three: round 1 blocked on two majors (the
refusal was not sticky, and the record kept only the last attempt); round 2 passed; round 3
verified the sweep's fix.
