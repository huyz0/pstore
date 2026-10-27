# M142 verification

The spec was reviewed in one round, and the code in one, pass with two minors, both
taken; both are recorded in the [spec](SPEC.md).

How to run the tests:
- unit: `cargo test -p pstore-server --lib rerank::`;
- server: `cargo test -p pstore-server --test it -- rerank:: composition:: snapshot::`.

Red runs: criterion 1's tests failed to compile before `Outer` and `fuse_outer` existed
(`cannot find type Outer`), which the spec counts as red. Criteria 2, 3 and 6 failed on the
parent, which refused `rerank_by` as a root key ("queries is the whole request"). Criterion
4's test passed on the parent for the same reason, so its red is the mutations below; the
refusals no mutation names (the model reranker's object, a non-integer `rank_constant`,
`offset` or `limit`, an unknown parameter, `offset` without `rerank_by`, a `limit` object
without `total` or with another key) are guards. Criterion 5 is a guard.

The mutations were applied one at a time by a script and then reverted, each run once
against the unit tests and once against the server's `rerank::` and `composition::` tests.
All 23 runs were killed. No `cargo mutants` run is claimed for them.

1. `fusion_equals_a_reference_over_random_rankings`, beside
   `defaults_are_sixty_weights_of_one_and_every_row`,
   `a_row_carries_the_first_attributes_and_no_dist_or_highlight`,
   `offset_is_applied_before_limit` and `a_limit_object_means_its_total`: green after.
   Killed by "rank counted from 0", "weights in reverse query order", "the maximum, not the
   sum", "duplicates kept", "ties by id descending", "ties by first appearance" and
   "offset applied after limit".
2. `the_fused_answer_is_the_reference_over_each_querys_own_answer`: green after. Its
   queries are a dense query asking for `n`, a text query asking for `tag`, and a
   `rank_by` query with its own `offset`; the dense and text answers overlap, which the
   test asserts. Killed by "rank counted from 0", "weights reversed", "the maximum",
   "duplicates kept" and "offset after limit".
3. `equal_fused_scores_order_by_id_whatever_the_query_order` (server) and
   `equal_scores_order_by_id_whatever_the_query_order` (unit): green after. Killed by
   "ties by id descending" and "ties by first appearance".
4. `what_cannot_mean_an_outer_rrf_is_refused_before_any_blob_request`, which also checks
   no refusal made a blob request, and the unit `what_cannot_mean_an_outer_rrf_is_refused`:
   green after. Killed by "a weight of 0 accepted", "a weight over 1,000,000 accepted", "a
   `rank_constant` of 0 accepted" and "a weights count mismatch accepted"; the server test
   also by "`limit` without `rerank_by` accepted".
5. `composition::` and `snapshot::` passed unedited (16 tests with `rerank::`), and
   `cargo test -p pstore-server --test it` passed (433). The server's library tests passed
   but for the two IPv6 tests this container cannot run.
6. `fusion_adds_no_depth_and_no_request`: green after; the same depth and request count,
   by `DepthCounting`, with and without `rerank_by`, each on a cold server, and both
   greater than 0.

Gates, in a cloud container:
- `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings`
  passed.
- ⚠️ Coverage: OBSERVED-NOT locally; llvm-cov in this container drops counts (see
  [M136](../M136/VERIFIED.md)). The floor is CI's run on push.
- NOT-RUN here: `scripts/mutants.sh` (`cargo-mutants` is not installed in this container).
