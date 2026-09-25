# M8j — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux cloud container (4 cores, 15 GB, `cargo-mutants` 27.1.0) that
restarts every few hours and wipes everything unpushed — which is why the measurement is
sharded and committed in [`sweep/`](sweep/), and the re-check against the workspace is a
separate committed step.

⚠️ **How "killed" was verified:** `cargo mutants` applied each named mutant one at a time
against `pstore-index`'s tests (`--test-workspace=false --test-package pstore-index`); a build
failure scores *unviable*, only a test failure *caught*, and each log was read for the test
that FAILED. Run on the code **before** the rung-1 changes, whose line numbers the names use.

1. **`dirty` deleted** — `lire.rs` has no `dirty` set; the three test comments that described
   it are rewritten, and the staged diff of `tests/lire.rs` outside comments is additions only
   (confirmed by code review). `cargo test -p pstore-index`: every test passes.
2. **One loop condition, no early exit** — split_pass is one `while let` with one increment;
   the round loop runs its fixed number of passes with no pre-check. Code review traced the
   old and new loops to identical `Clustering` and `Work` in every case, and
   `cargo test -p pstore-index` passes unchanged.
3. **The split bound** — `a_list_at_the_split_bound_is_left_whole_and_one_past_it_is_split`.
   ⚠️ **OBSERVED-NOT killing 48:49 on the old code**, and that is the finding: the pre-check
   `any(.. > max)` never called the pass for a list of exactly `max`, so it masked the mutant.
   Whether it is killed now is criterion 7.
4. **The threshold and the neighbour** — `a_centroid_that_moves_exactly_the_threshold_is_not_disturbed`
   killed 304:49 `>`→`>=` and 314:69 `!=`→`==`. The edge value `f32::from_bits(0x3D01_7712)`
   was found by an f32 model and re-derived independently by both reviewers.
5. **The misplaced row** — `reassignment_moves_a_row_to_the_list_it_is_nearest` killed 340:25
   `!=`→`==` (the threshold test failed on it too).
6. **`bisect`** — `bisect_is_lloyd_from_the_farthest_pair_with_ties_to_the_first_seed` killed
   449:30 `<=`→`>`.
7. **The confirming run** — NOT-RUN yet: every `lire.rs` mutant on this tree against
   `pstore-index`'s tests is running; recorded in a follow-up.
8. **The full gate** — `./scripts/gates.sh` on this tree: all fifteen gates PASS.
