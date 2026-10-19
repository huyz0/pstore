# M108 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, against `MemoryStore`. Request counts and
byte counts are exact. Nothing here is a wall-clock number, and nothing measures a real fleet's
record sizes, which is the half of OQ-8 a deployment answers.

1. **The default threshold, against the design point.**
   `the_default_splits_past_twice_the_design_point_and_not_at_it`
   (`cargo test -p pstore-catalog --test threshold -- --nocapture`) prints, at 10,737 bytes a
   record:
   - `1000000 tenants: fullest bucket 99 tenants, mean 61.0, skew 1.62, 1062963 bytes against 2097152`
   - `2000000 tenants: fullest bucket 169 tenants, mean 122.1, skew 1.38, 1814553 bytes against 2097152`
   - `2500000 tenants: fullest bucket 208 tenants, mean 152.6, skew 1.36, 2233296 bytes against 2097152`

   Under at 1M and 2M, over at 2.5M, as asserted. Its first run was red on the record size
   (`10743` against `10737`): integer division of `1_000_000 / 16_384` truncated the mean to
   61, and the test now computes `655_360 × 16_384 / 1_000_000`.
2. **Below the threshold nothing is written.** `below_the_threshold_nothing_is_written`:
   `NotDue` with each run's size equal to a direct read of it, `1 + 2·4` reads, 0 writes,
   0 LISTs, and the root unchanged. ⚠️ This passed against the never-split stub as well, which
   is what the spec's test plan expected: it is the stub's own answer. Its red is criterion 3's.
3. **Above it, one split, and no extra read round.**
   `above_the_threshold_it_splits_on_the_census_it_decided_on`: `Split { from: 4, to: 8 }`, the
   root at 8, all 400 tenants found at 8, and exactly `1 + 2·4` reads. Seen red twice: against
   the stub (`NotDue(… fullest_bytes: 4396)`, not `Split`), and with the read before each
   sibling write put back (`13` reads, not `9`).
4. **At the format, it says so.** `at_the_format_it_says_so`: at width 65,536 with one tenant
   folded and a 1-byte threshold, `AtFormat` names that tenant's bucket and the root is
   unchanged. Seen red against the stub (`NotDue at a 1-byte threshold`).
5. **A record appended between two splits survives.** `a_record_appended_between_two_splits_survives`:
   splitter B reads the root as it was before A's flip, and tenant 10002 is recorded at width 8
   into a sibling bucket. B then fails with `RootContended`, and an enumeration at 8 finds
   10002 and the 400 others. Seen red against the old `split`, which read and overwrote the
   sibling head: `tenant 10002, appended at the new width, was lost to a stale splitter`.
6. **A re-run after a crash still completes.** `a_rerun_after_a_crash_still_completes`: the
   root write fails once after the siblings are written (sibling 4's head exists, the width is
   still 4). A second `split` returns 8 and all 400 tenants are found. Green before and after
   the change, as the spec says: it guards M6g's resume against the create-if-absent rule.
7. **Zero LIST.** `below_the_threshold_nothing_is_written` and
   `above_the_threshold_it_splits_on_the_census_it_decided_on` assert 0 LISTs on the
   accounted view.
8. **The roadmap's line and M6g's gap** are struck, linking here.
   `grep -c "Decided by \[M108\]" docs/research/11-design/roadmap.md` → 1;
   `grep -c "decided by \[M108\]" docs/milestones/M6g/VERIFIED.md` → 1. ⚠️ M6g's line says what is
   still true: nothing calls the split on a schedule.
9. **Gates.** `./scripts/gates.sh`, by the pre-commit hook on this commit.
   - **Review round 1** failed on one major: this line promised mutation evidence that did not
     exist yet. It is below. Of its minors, one was taken: a 409 on a sibling's head create was
     read as a lost create, so the split flipped to a bucket with no head.
     `a_contended_sibling_head_fails_the_split_before_the_flip` was red on that (`Width(8)`,
     not an error) and is green now that only a 412 is success. M6g's line about a re-run
     redoing the partitioning carries a correction banner.
   - **Mutants.** `./scripts/mutants.sh` over the diff: 18 mutants. The first two runs died on a
     full disk. The second recorded 17 outcomes before it did: 11 caught, 6 unviable, 0 missed.
     The 18th, `>` to `>=` in `Occupancy::of`'s tie-break, ran alone
     (`MUTANTS_JOBS=1 ./scripts/mutants.sh -F 'replace > with >= in Occupancy::of'`) and timed
     out, which is not a kill. `a_tie_names_the_lowest_bucket` pins it. It was red under the
     mutant by hand (`fullest: 3`, not `0`), and the same command then read `1 mutant tested
     in 9m: 1 caught`.
