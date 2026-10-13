# M67 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container with 4 cores. No latency is claimed; the
recall figure is on a seeded synthetic corpus and **provisional**.

Commands:
- `cargo test -p pstore-format --test fde`: 9 tests.
- `cargo test -p pstore-query --test multi`: 6 tests.
- `cargo test -p pstore-server --test multi_fde`: 5 tests.
- `cargo test -p pstore-engine --test multi`

1. **The encoding is the algorithm.** In `crates/pstore-format/tests/fde.rs`:
   `fde_is_deterministic_and_fixed_width` (512 floats for `d` of 8 to 128 and 1 to 1,000
   vectors; a query scales linearly, a document of one vector twice is that vector once),
   `fde_with_one_bucket_is_sum_and_mean` (by hand: mean `[2, 2, 1]`, sum `[1, 3, 0]`, dot 8),
   `fde_fills_an_empty_bucket_from_the_nearest`,
   `fde_fills_an_empty_bucket_by_hamming_distance_not_by_order` (the empty bucket takes the
   *later* vector when it is one bit away and the earlier is two),
   `fde_buckets_are_balanced_and_nearby_vectors_share_one` (800 vectors over 8 buckets, each
   40 to 220; a small nudge keeps the bucket over 90% of the time, an unrelated vector
   under 25%), `fde_projection_keeps_norms_roughly`.
2. **It finds what exact MaxSim finds.** `fde_candidates_recover_the_exact_top`: on a seeded
   clustered corpus (300 documents of 8 to 24 vectors, `d = 32`, 20 queries), **recall of the
   exact top 10 within the encoding's top 100 is 0.98** (floor 0.9, provisional), and a
   control ranker over another corpus's encodings reaches 0.40.
3. **Through the API.** In `crates/pstore-server/tests/multi_fde.rs`:
   `fde_mode_ranks_and_exact_mode_is_unchanged` (80 documents over two segments, a compaction
   and a third segment; at least 7 of the exact top ten within the encoding's top thirty, for
   three queries; `mode: "exact"` equals no mode),
   `rerank_exact_orders_by_maxsim` (a window covering the corpus gives the exact order and
   scores),
   `fde_reads_less_than_the_exact_leg` (64 vectors of 128 dimensions: **100,250 bytes against
   1,329,386**, 13 times fewer, at the same depth; the rerank at a window of 10 reads less than
   the exact leg and is at most two rounds deeper),
   `the_derived_field_is_not_a_name_a_client_may_write`. In
   `crates/pstore-query/tests/multi.rs`:
   `a_segment_without_the_encoding_answers_nothing` (a segment without `late$fde` answers
   nothing in `fde` mode and fully in exact mode) and
   `fde_scores_are_the_encodings_dot_and_shadowed_rows_are_replaced` (scores bit-equal to the
   test's own dot; a limit of 3 met past two shadowed rows).
   In `crates/pstore-engine/tests/multi.rs`, `two_writers_widths_meet_in_the_fold`: the
   quarantine export carries no `$fde` field. Seen red with `client_form` not stripping it.
4. **Refusals.** `fde_options_are_refused_at_the_door`: 8 option refusals, `rerank` without
   `fde` mode, `rerank` beside filters, the limits accepted, and a filter without `rerank`
   served.
5. **The reader.** `field_rows_reads_only_the_named_rows` (a duplicate, the row at and past
   the end dropped, rows holding no vectors empty, a fixed-width field; **under a quarter of
   the field's bytes** moved) and `field_rows_refuses_a_corrupt_table_and_skips_a_sparse_field`
   (an offset past the section, above the next, and a non-ascending one are errors; a sparse
   field returns nothing).
6. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - **Mutation:** `cargo mutants --in-diff` over the changed lines, `--no-config`, per crate:
     - `pstore-server` `lib.rs`: 14 mutants, 13 caught, 1 unviable, 0 missed.
     - `pstore-query` `run.rs`: 18 mutants, 8 unviable, 10 caught after three tests were
       added: the `MultiFde` arm of `widened` (`+` as `-`) and the leg's dot product (`*` as
       `+`, `/`), killed by `fde_scores_are_the_encodings_dot_and_shadowed_rows_are_replaced`.
     - `pstore-format` `fde.rs` and `reader.rs` (`--test fde --lib`): the first sweep, 146
       mutants, missed 43. Tests were added for the behaviour among them (row == end, corrupt
       offset tables, rows holding no vectors, the Hamming fill, a zero vector's bucket, the
       projection scale, `the_encoding_is_frozen`, and `gaussians_and_signs_are_frozen`, which
       pins four gaussians to `1e-9` and eight signs and checks the draws are standard normal).
       Six mutants were equivalent as written, so **the code was restructured to remove them
       rather than argue them**: the gaussian's unit interval is 24 bits, not 53 (a `+ 0.5` as
       `- 0.5` now moves a draw by `2^-24`, which the golden values see); the projection's sign
       is applied by `signed(a, ..)`, not multiplied (no `*` as `/`); the bucket id is built with
       `+`, not `|`; the output vector is no longer pre-sized; and `field_rows`' guards are one
       condition each (`kind != 0`, `wanted.is_empty()`, `dims == 0`). The 24-bit interval
       changes no encoding in the tests (the planes' signs are unmoved), and no encoding was in
       use. Final sweep: **139 mutants, 138 caught, 1 missed** (`||` as `&&` in the combined
       guard), which the split removed; re-swept: `field_rows`, **45 mutants, 45 caught**.
       **0 misses.**
     - `pstore-engine` `lib.rs`: 20 mutants (11 caught, 7 unviable, 2 missed), and the two
       were killed by tests added after: `write_marked`'s `!vs.is_empty()` guard by
       `a_named_field_holding_no_vectors_derives_no_encoding`, and `shares` (replaced by
       `vec![]`) by the peers tests, with the `fde` cases added to `a_multi_query_is_not_split`.
       Re-swept with `pstore-engine --test multi` and the server's `peers` and `multi_fde`:
       `write_marked` and `shares`, **7 mutants, 6 caught, 1 unviable, 0 missed.**
     - **`pstore-format` `fde.rs` and `reader.rs` whole, on the final tree** (after the last edit): **138 mutants, 138 caught, 0 missed.**
     - **Re-swept on the final tree** (after the encoding's restructuring, `bffd251`), changed
       lines only: `pstore-query` `run.rs`, 14 mutants, 9 caught, 5 unviable, 0 missed;
       `pstore-server` `lib.rs`, 14 mutants (a worker restart split the run; 9 then 5),
       **0 missed**.
     - Every crate's sweep therefore ends with **0 misses** against the tests named above.

**Code review**, one round (an agent given the staged diff and not the author's reasoning).
- **Major, fixed:** the quarantine export (`client_form`) returned the derived field, chopping
  its last float under `euclidean_squared`. It now drops every `$fde` field; see AC3.
- **Noted, minor:**
  - `rerank` beside `filters` is refused, which the spec's rule 2 now says;
  - with a boost or diversify window, the leg's limit is that window, and `rerank_window` is
    bounded below by it;
  - the fixed-width branch of `field_rows` does not bound a row by the section's end, as the
    variable-width branch does; `decode_field` does likewise;
  - the query's planes and signs are recomputed per segment, and a document's encoding costs
    about 65 thousand hash mixes per named field at 128 dimensions, on the write path;
  - SimHash runs over the transformed space, where euclidean's appended `-|v|²/2` coordinate
    can dominate a hash: a recall concern the tests do not exercise.

**Not covered**, as the spec states: configurable parameters, quantizing the encoding, an
index over it (row 65), splitting across peers (row 64), and rebuilding old documents.
