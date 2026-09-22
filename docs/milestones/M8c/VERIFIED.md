# M8c — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

All runs are in the Linux dev container. The red for criterion 3 is CI's: nightly run
35702526909, shard 0, on `25089c7`, reported these eight MISSED; `faulty.rs` was unchanged
between that commit and this change.

1. **The generator and the exclusion's premise are pinned** —
   `split_mix_matches_the_reference_sequence` asserts SplitMix64's published first two outputs
   from state 0 on the 53 bits `split_mix` returns, and `no_faults_is_exactly_the_default`
   asserts `Faults::none() == Faults::default()` (`cargo test -p pstore-blob --lib faulty`).
   **Mutation verified killed**: `z ^= z >> 31` → `|=` applied by hand fails the first test
   with an assertion at the reference value, not a build error.
2. **A draw on the rate does not fire, one below it does** —
   `a_draw_equal_to_the_rate_does_not_fire_and_one_below_it_does`, a fresh `Faulty` per case.
   **Mutations verified killed, each applied by hand and one at a time**: all six `<` → `<=`
   (`read_fault` ×2, `write_fault` ×2, `cas_fault` ×2), and a `<` → `>` control on the first,
   which only the above-rate half catches. The failure was confirmed to be the assertion
   panicking, not a compile error.
3. **No survivor left in the file** —
   `./scripts/mutants.sh --file crates/pstore-blob/src/faulty.rs`: 89 mutants tested in 20m,
   **74 caught, 15 unviable, 0 missed**. Red first: 8 missed in CI's nightly on this file.
4. **The exclusion removes exactly one mutant** — `cargo mutants --list` prints 3,400 lines,
   and the same command with `--no-config` prints 3,401; the diff is the single line
   `crates/pstore-blob/src/faulty.rs:54:9: replace Faults::none -> Self with Default::default()`.
5. **The full gate** — `./scripts/gates.sh` in the dev container: all fifteen gates PASS.
