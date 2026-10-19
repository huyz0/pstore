# M109 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, against `MemoryStore`. Request counts are
exact. Nothing here is a wall-clock number. Nothing is wired: the server constructs no `Meter`,
so no production answer is counted yet.

⚠️ **How each was seen red.** Every test passed on its first run against the implementation,
so each one's red is a **seeded defect**: one change to `report.rs`, run against that test
alone, then reverted. Each seed is named below with what the test printed.

1. **The fleet's counts are the sum of the nodes'.** `the_fleets_counts_are_the_sum_of_the_nodes`
   (`cargo test -p pstore-meter --test rollup`): three nodes, and tenant A's answers sum to
   63 with 3 failed, B's to 12. Its reads and bytes sum too (9 and 300). Red with the rollup
   reading only the first node the index names: `["n1"]` against `["n0", "n1", "n2"]`.
2. **A node that left is still counted.** `a_node_that_left_is_still_counted`: a node that
   published once and was dropped is in the rollup, with no member list passed. Red with
   `publish` not registering: `the index lost a node that left`, `[]` against `["gone"]`.
3. **A restart continues the month.** `a_restart_continues_the_month`: a fresh `Meter` and
   `Reporter::resume` continue the report, `(18, 3)`. Red with `resume` keeping the tag and
   dropping the stored counts: `(7, 2)`, the first half lost.
4. **A new month starts at zero.** `a_new_month_starts_at_zero`: March keeps `(11, 1)`, April
   holds `(4, 1)` and then `(5, 1)` after a second April publish. Red with the base not cleared
   at the turn: `(15, 2)` against `(4, 1)`.
5. **Two processes with one node id are refused.** `two_processes_with_one_node_id_are_refused`:
   the second publish fails `Contended("n1")` and the rollup still reads the first one's
   `(10, 0)`. Red with an unconditional `put`: `a second process overwrote the first one's
   report`.
6. **What it costs.** `a_publish_is_one_write_and_a_rollup_is_one_read_per_shard_and_per_node`,
   on an accounted view with 5 nodes:
   - the month's first publish costs 1 read and 2 writes (the report, the shard's read and CAS);
   - a later publish costs 0 reads and 1 write;
   - the rollup costs exactly `64 + 5` reads and 0 writes;
   - 0 LIST.

   Red with the registration never remembered: the later publish read the shard again,
   `(1, 1)` against `(0, 1)`. A first seed, on the already-registered branch, survived. That
   branch is only taken after a resume, so the seed was moved to the branch this test takes.
7. **The budget arithmetic.** `the_budget_allows_the_target_and_not_one_failure_more`: at
   999,000 ppm over 10,000 answers, 10 failures leave `remaining: 0, exhausted: false`, and 11 leave
   `remaining: -1, exhausted: true`. A tenant nobody served is `0, 0, false`. Red with `>=` for
   `>`: `exhausted: true` at 10. `four_nines_over_ten_thousand_answers_allows_one_failure`
   (review round 1) was red against the float target: `allowed: 0, remaining: -1, exhausted:
   true`. It is green in integers.
8. **Month arithmetic.** `a_month_is_the_utc_calendar_month`: the epoch is 1970-01. Either side
   of 2024-02-29T23:59:59Z is 2024-02 and 2024-03, and either side of the 2000 new year is
   1999-12 and 2000-01. Red with the January and February year correction removed: 1969-01.
   `keys_carry_the_month_and_the_identity` (`cargo test -p pstore-meter --lib`) pins
   `use/2026-03/node/n1` and `use/2026-03/idx/0a`, and checks that 1,000 node ids use all 64
   shards. `a_report_and_an_index_round_trip_and_a_trailing_byte_is_corrupt` pins the
   encodings and that a truncated, overlong or wrongly tagged object is `Corrupt`.
9. **`slos.md`.** The row is split. The rollup is `enforced`, naming
   `a_publish_is_one_write_and_a_rollup_is_one_read_per_shard_and_per_node` and
   `a_node_that_left_is_still_counted`. The target stays `blocked`, on a real fleet's failure
   rate and on the server calling `Meter::answer`. `./scripts/check-slos.py` →
   `ok 25 objectives, every enforced one names a gate that exists`.
10. **Gates.** `./scripts/gates.sh`, by the pre-commit hook on this commit.
    - **Review round 1** failed on two majors, both fixed above or here:
      - The float budget, under criterion 7.
      - A report write that landed and then answered `Io` left the reporter with a tag the
        store had moved past, so every later publish was refused as `Contended`.
        `a_write_that_landed_and_timed_out_does_not_wedge_the_reporter` was red on exactly
        that (`Contended("n1")`). It is green now that the next publish reads the report once
        and adopts it when it holds that write's bytes.
      - `a_write_that_never_landed_is_counted_once_and_never_taken_for_anothers` was red with
        the byte check removed: another process's report was overwritten.
    - **Mutants.** `./scripts/mutants.sh --test-package pstore-meter` over the diff: `125
      mutants tested in 3m: 3 missed, 108 caught, 14 unviable`. All of this code and every test
      of it are in `pstore-meter`. A whole-workspace run on one job was stopped after 7 outcomes,
      since at that rate it would have taken about eight hours.
    - The 3 missed were all in `Month::of`'s century terms (`doe / 36_524` and
      `doe / 146_096`), which no date before 2100 exercises.
      `every_day_to_2500_falls_in_the_month_a_walked_calendar_says` checks every day from 1970
      to 2500 against a calendar walked month by month. By hand it was red under the first two
      (day 59 and day 11016), and
      `./scripts/mutants.sh --test-package pstore-meter -F 'in Month::of'` then read `50 mutants
      tested in 2m: 50 caught`.
    - **After round 1's fixes**, the same sweep over the whole diff:
      `./scripts/mutants.sh --test-package pstore-meter` → `131 mutants tested in 3m: 117
      caught, 14 unviable`, 0 missed.
