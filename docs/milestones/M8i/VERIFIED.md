# M8i — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux cloud container (4 cores, 15 GB, `cargo-mutants` 27.1.0), not
the compose dev container — none was available. Same OS and toolchain; `mutants.sh` sized itself
to 2 jobs from the cgroup.

⚠️ **How "killed" was verified.** Not by editing each file by hand: `cargo mutants` applied each
of criterion 1's 62 surviving mutants **one at a time** against `pstore-testkit`'s own tests
(`--test-workspace=false --test-package pstore-testkit`, exact-name `-F` filters), which scores a
build failure as *unviable* and only a test failure as *caught*. All 62 were **caught**, and each
mutant's log was read for the test that FAILED — the test named below, in every case.

1. **The measurement** — `./scripts/mutants.sh --check pstore-testkit --file 'crates/pstore-testkit/src/*.rs'`
   on `0337112`, before any change: 303 tested in 3h, **64 missed**, 166 caught, 73 unviable.
   All 36 of the nightly's are among them; the other 28 are the spec's amendment — shards 1
   and 3 of the nightly never finished, so the nightly had never measured them.
2. **`Sim`** — `the_generator_is_splitmix64_on_all_64_bits` (killed all 6 finaliser mutants;
   its values from an independent Python model whose first two outputs match the published
   ones), `steps_counts_every_decision` (`steps -> 0`, `-> 1`, `+=`→`*=`),
   `chance_fires_below_p_and_not_at_it` (`<`→`<=`, and the 6 finaliser mutants again).
3. **`Flaky`'s draw** — `a_rate_refuses_below_it_and_not_at_it_on_every_pinned_draw`: killed all
   8 mixing mutants and `<`→`<=`.
4. **`Gated`** — `a_lone_writer_at_an_armed_gate_waits_it_out_and_is_not_a_race`: killed
   `raced -> true`, `<`→`==`, `<`→`>` and `arm -> ()`.
5. **`render`** — `a_row_is_flagged_exactly_when_it_reaches_the_budget`: killed all five
   (`||`→`&&`; `>`→`==`, `<`, `>=`; `>=`→`<`).
6. **Counters** — `keys_written_counts_distinct_keys` (`keys_written -> 1`);
   `claims_hands_back_the_objects_it_holds` (`store -> Default::default()`).
7. **The two exits** — `each_early_exit_abandons_exactly_one_commit`: killed `-=` and `*=` on
   both the `Io` and the `Ok(None)` exit.
8. **`Broken`'s `_as` reads** — `a_broken_backend_carries_its_defect_on_the_class_carrying_reads`:
   killed both `==`→`!=`, and also `Broken`'s `get_range_as`/`get_suffix_as` `-> Ok(Default)`.
9. **The sweep configurations** — `a_latency_sweep_runs_the_spread_it_records` (both
   `latency_min` and `latency_max` deletions; passes on correct code at exactly 88 ms, per the
   spec's closed bound), `a_cas_error_sweep_runs_the_rate_it_records` (`cas_lost` deletion).
10. **Forwarding** — `every_double_forwards_what_it_does_not_change`: killed 15 of the 17
    constant-`Ok` mutants. ⚠️ **Drift from the spec, recorded:** the other two, `Broken`'s
    `get_range_as` and `get_suffix_as`, were caught first by criterion 8's test, which the
    one-at-a-time run stops at; the forwarding test also calls both.
11. **The probes compare** — `each_probe_compares_the_answer_and_not_just_its_shape`: killed all
    four guard → `true` mutants. `the_seed_printed_with_a_failure_is_the_one_that_replays_it`:
    killed `seed -> 0` and `-> 1`.
12. **The deletion** — `refusing_reads` sets only `reads_fail`; `cargo mutants --list` over
    testkit goes from 303 to **301**, the two missing being the two deleted-field mutants.
    `refusing_reads_refuses_every_kind_of_read` passes (`cargo test -p pstore-testkit`).
13. **The confirming sweep** — NOT-RUN yet: pending the sweep on this tree.
14. **The full gate** — `./scripts/gates.sh` on this tree: all fifteen gates PASS.
