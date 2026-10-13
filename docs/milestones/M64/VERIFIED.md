# M64 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container with 4 cores. No latency is claimed;
depth, requests and bytes are exact.

Commands:
- `cargo test -p pstore-server --test diversify`
- `cargo test -p pstore-query --test vectors`
- `cargo test -p pstore-engine --test vectors`
- `cargo test -p pstore-format -p pstore-query -p pstore-engine -p pstore-server`

1. **MMR is MMR.** `mmr_picks_as_the_model_does`: under cosine, euclidean and dot product,
   over clustered near-duplicates of unequal norms, unfolded, folded and mixed, the ids equal
   the test's own `f64` greedy MMR for `lambda` 0, 0.3, 0.7 and 1, with every choice's margin
   asserted above `1e-5`. Seen red with `diversify` absent (the field unknown).
2. **Ties and a missing vector.** `mmr_ties_keep_the_earlier_candidate`, by hand
   `d00, d03, d01, d02`; and the unit tests in `diversify.rs`:
   `similarity_is_zero_without_a_usable_vector`, `a_negative_similarity_is_a_bonus`,
   `a_range_that_is_flat_or_not_finite_gives_every_candidate_relevance_one`,
   `lambda_one_is_relevance_and_lambda_zero_starts_with_the_first`,
   `ties_keep_the_earlier_candidate_and_a_short_window_returns_what_it_has`.
3. **With a boost.** `diversify_picks_from_the_boosted_window`: boosted scores and window are
   MMR's; a window named on one governs both; different windows are refused.
4. **Depth and cost.** `vectors_ride_the_row_round`: four rounds at one segment and at four,
   folded, and with unfolded rows whose shadow check fetches every candidate's vector; bytes
   above the `top_k = window` query's and within the vector sections.
5. **Refusals.** `diversify_is_refused_at_the_door`.
6. **The query layer.** `vector_rows_ride_with_the_rows` and
   `a_segment_whose_vectors_section_is_not_vector_resolves_without_one`.
7. **The engine.** `client_space_is_the_same_for_unfolded_and_folded_rows`,
   `a_hit_whose_row_vanished_does_not_misalign_the_vectors`,
   `vectors_are_empty_unless_asked_for`.
8. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - **Mutation:** `cargo mutants --no-config --in-diff` over the changed source lines, per crate, each
     against its own crate's tests (the server's `--test diversify --test boost --lib`):
     - `pstore-server` (`diversify.rs`, `lib.rs`, `boost.rs`): 76 mutants, 68 caught, 5 unviable,
       **3 missed**, resolved below.
     - `pstore-query/src/run.rs`: 34 mutants, 2 missed (below); `pstore-format/src/reader.rs`:
       7 mutants, all caught by `pstore-query`'s `tests/vectors.rs`; `pstore-engine/src/lib.rs`:
       19 mutants, 13 caught, 6 unviable. ⚠️ `split_rows`' whole-function replacements by
       constant five-tuples (hundreds of combinations) were excluded as a class.
     - **The 3 server misses:**
       - `cosine`'s `na == 0.0 || nb == 0.0` as `&&`: one zero norm gave 0/0 = NaN, and a NaN
         objective never beats a number, so a zero-vector candidate that outranks the rest
         lost. Killed by a case added to `similarity_is_zero_without_a_usable_vector`
         (candidate order matters: NaN wins only when first); seen red by applying the mutant.
       - `select`'s `(s - lo)` as `(s + lo)`: ⚠️ equivalent. It adds the constant `2·lo/(hi-lo)`
         to every candidate's relevance, which moves every objective by the same amount and
         changes no argmax.
       - `lib.rs` `h.segment == answer.unfolded_at` as `!=` in `unfolded_hits`: code this change
         only moved, and the M62 ledger records its killers; they were not in this sweep's
         selection.
     - **The 2 query misses,** `summed` and `query_rows_filtered` replaced whole: their only
       change here is the `with_vectors` parameter. Re-run against `pstore-query` and
       `pstore-engine` tests: all caught.

**Not covered**, as the spec states: `as_of`, named fields' vectors, sparse vectors,
returning the picked vectors, and attribute diversity.

**Code review**, one round (an agent given the staged diff and not the author's reasoning):
no blocking or major findings. **Noted, minor:** similarity is always the `vector` field's,
whatever field the dense leg ranked on; with `field` set to another, or no `vector` field,
every similarity is 0 and `diversify` is a relevance cut at `top_k`. The docs say so.
