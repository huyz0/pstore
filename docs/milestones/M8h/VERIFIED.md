# M8h — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

All runs are in the Linux dev container. Every "killed" was a mutant applied by hand, one at a
time, restored afterwards, and confirmed to fail the named test's assertion, not the build
(`cargo test -p pstore-cluster`).

1. **`median`, directly** — `the_median_of_odd_and_even_and_empty_slices` asserts 2, 2.5, 5, 3
   and 0 on all 64 bits. **Mutations verified killed**, all seven: `==` → `!=`, `%` → `/` and
   `+` on the parity test; `/` → `%` and `*` on the odd index; `-` → `/` and `/` → `%` on the
   even pair.
2. **Three zones is a floor, not the only size** — `outliers_need_three_zones_but_not_exactly_three`:
   four zones name the degraded one, two zones with a degraded one name nobody. **Mutation
   verified killed**: `samples.len() < 3` → `> 3`.
3. **The three edges** — `a_zone_exactly_at_the_latency_threshold_is_not_an_outlier`,
   `a_zone_exactly_at_the_success_margin_is_not_an_outlier`,
   `a_survivor_exactly_at_the_headroom_blocks_a_drain`, each asserting the edge value and one
   float past it. **Mutations verified killed**: the latency `>` → `>=`, the success `<` → `<=`,
   and the survivor `<` → `<=`.
4. **Three redundancies deleted** — the `% ring.len()` on the ring search's start, the outer
   relax-loop `if`, and — found by the first confirming sweep, and created by the first
   deletion — the early return for an empty ring or `r == 0`, whose `||` → `&&` mutant had been
   caught only by a division by zero that no longer exists. Each has a comment saying why;
   `place(key, 0)` is now asserted empty. Every existing placement test passes
   (`cargo test -p pstore-cluster`), including the golden vector.
5. **The hash, on all 64 bits** — `the_hash_matches_an_independent_model`. **Mutation verified
   killed**: the last finaliser `h ^= h >> 33` → `|=`, which only moves the low 31 bits and
   so had passed the golden placement test.
6. **One exclusion, exactly one mutant** — `.cargo/mutants.toml` names
   `placement.rs:77:54: replace < with <= in Placement` (76:54 before the guard's deletion moved
   it). `cargo mutants --list` on the final code: **3,368** without the line, **3,367** with it,
   and the only difference is that mutant. The
   entry states that it is not a proven equivalence and has no premise test, and why.
7. **No survivor in the crate** — `./scripts/mutants.sh --check "^crates/pstore-cluster/src/"`:
   136 tested in 11m, **0 missed in `pstore-cluster`**. (The first confirming run found the
   early-return guard of criterion 4; this is the run after its deletion.) The regex also selects
   `pstore-testkit` mutants through their descriptions; the 5 missed there are the queued
   `pstore-testkit` work.
8. **The full gate** — `./scripts/gates.sh` in the dev container on this tree: all fifteen gates PASS.
