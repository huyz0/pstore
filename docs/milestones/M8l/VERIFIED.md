# M8l — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0). Recall
numbers are `provisional`.

⚠️ **How "killed" was verified:** each named mutant was applied by hand, one at a time, and the
one test run and seen FAILED (by the author, and again independently by the round-1 reviewer);
then the full sweep in criterion 4. Line numbers are the measurement's.

1. **The defect** — `every_rung_zero_score_is_the_rows_code_against_its_own_lists_centroid` was
   **observed red** on the code as it was (`d2 (2 copies) scored 0.15838747, its codes give
   0.18224691`) and passes after the fix (`cargo test -p pstore-index --test persisted`). Its
   fixture asserts it replicates at least one document.
2. **Each named mutant killed**, by the test named:
   `every_int8_score_is_the_rows_own_int8_estimate` (`704:59`, `d1 scored -inf`);
   `a_field_no_document_carries_writes_no_code_bytes` (`380:16`, `RaBitQ holds 5400 bytes`);
   `the_slack_is_a_fraction_of_the_nearest_distance` (all three `search.rs:50`);
   `the_assignment_cost_is_the_squared_distance_to_the_assigned_centroid` (the three `124:9`
   constants); `a_seeding_tie_goes_to_the_first_row` (`247:23`);
   `a_full_list_takes_no_more_rows` (`346:66`, which also fails the two boundary tests);
   `a_zero_boundary_replicates_nothing_not_even_an_exact_tie` (`271:29`, `271:55`);
   `two_lists_are_enough_to_replicate_between` (both `271:74`);
   `a_range_too_narrow_for_a_step_codes_like_a_constant_vector` (`sq8.rs:41:21`, codes
   `[0, 255]`). `363:40`'s code no longer exists; its behaviour is criterion 1's test.
3. **The rewrites** — `grep -n "hi > lo\|\*first + \*len" crates/pstore-index/src/sq8.rs crates/pstore-index/src/vec_index.rs`:
   the sq8 comment, and `vec_index.rs:700` in `search` only. The step's equivalence was checked
   by a throwaway program, bit for bit over empty, constant, `±inf`, NaN, `±0`, subnormal and
   `±MAX` inputs (author), and exhaustively over 1–3-element vectors of those (reviewer).
   **NOT-RUN** as a test: the branch it compares against is gone.
4. **Every mutant in the four files** —
   `./scripts/mutants.sh --check pstore-index --file crates/pstore-index/src/cluster.rs --file crates/pstore-index/src/search.rs --file crates/pstore-index/src/sq8.rs --file crates/pstore-index/src/vec_index.rs --test-workspace=false --test-package pstore-index`
   on the final tree: **368 tested in 30m, 259 caught, 109 unviable, 0 missed, 0 timeouts**.
   No survivor, so no workspace re-run was needed. This closes M8j's 50 in `pstore-index`.
5. **Recall and the full gate** — `./scripts/recall.sh`: PASS (uniform p=16: none 0.284, fast
   0.738, exact 0.739 — the same as M8k's; provisional). `./scripts/gates.sh` on the final tree:
   all fifteen PASS.

**Drift, recorded:** comments in `a_clustered_query_returns_the_documents_it_should` and
`a_clustered_index_keeps_its_recall_in_suite` (`tests/persisted.rs`) still name the removed
`home` loop's `first..first + len` (round-1 review, minor; not fixed here).
