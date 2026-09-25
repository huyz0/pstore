# M8k — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, 15 GB, `cargo-mutants` 27.1.0).
The NaN's sign and ranking were measured here; aarch64 was not.

⚠️ **How "killed" was verified:** the final mutant run applied every `rabitq.rs` mutant one at a
time against `pstore-index`'s tests, and each mutant's log was read for the test that FAILED —
the names below. Line numbers there are the final tree's (the spec keeps the measurement's).

1. **The defect** — `a_vector_on_its_centroid_scores_the_centroid_not_nan` and
   `a_residual_too_small_to_have_a_norm_scores_the_centroid_not_nan` were each **observed red,
   scoring NaN**, before their fix; `the_zero_vectors_own_code_estimates_zero` red before the
   alignment guard. All three pass (`cargo test -p pstore-index --test rabitq`). A standalone
   `0 × ∞` on this machine: `0xffc00000`, sorted last by `total_cmp`.
2. **Each named mutant killed** — by the test the spec names, per the logs:
   `a_dimension_error_says_what_it_expected_and_what_it_got` (Display);
   `the_residual_is_the_vector_minus_its_centroid_and_is_encoded_as_a_unit` (the three
   `residual_norm` constants, `-`→`+`, `>`→`==`/`<`, `/`→`%`/`*`);
   `a_quantizer_reports_the_dimension_it_was_built_for_not_the_padded_one` (both `dim` constants);
   `an_exact_zero_coordinate_is_not_a_positive_bit` (`>`→`>=` in encode_unit);
   `an_int4_query_is_rounded_to_sevenths_of_its_largest_component` (`<=`→`>`, and both
   `estimate_unnormalized_for_test` constants);
   `a_residual_estimate_is_the_centroid_plus_the_scaled_unit_estimate` (`*`→`+`);
   `the_error_bound_is_epsilon_times_the_misalignment_over_the_remaining_dimensions` (both
   `D - 1` mutants); and `a_vector_on_its_centroid_scores_the_centroid_not_nan` (`norm > 0` →
   `>=`, through its stored-code assertions — **OBSERVED-NOT** before code review added them:
   the run before that fix reported exactly this one MISSED).
3. **Rotate and the exclusion** — rotate loops over `trailing_zeros`; `cargo mutants --list`
   over the file is 127 with the exclusion and 128 without it, the difference being
   `293:23 * -> /`. Premise: `every_sign_flip_is_exactly_plus_or_minus_one` passes.
4. **Every `rabitq.rs` mutant** — `./scripts/mutants.sh --check pstore-index --file crates/pstore-index/src/rabitq.rs --test-workspace=false --test-package pstore-index`
   on the committed tree: **127 tested in 15m, 119 caught, 8 unviable, 0 missed**. No
   survivor, so no workspace re-run was needed.
5. **Recall and the full gate** — `./scripts/recall.sh`: PASS (none 0.284, fast 0.738, exact
   0.739; provisional). `./scripts/gates.sh` on the committed tree: all fifteen PASS.
