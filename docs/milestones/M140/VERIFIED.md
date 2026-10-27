# M140 verification

The spec was reviewed in one round, recorded in the [spec](SPEC.md); the code in one,
pass with two minors, both taken (the `rank_by` field's doc, and a test count here).

How to run the tests:
- unit: `cargo test -p pstore-query --lib order::`;
- engine: `cargo test -p pstore-engine --test it order::`;
- server: `cargo test -p pstore-server --test it -- order_keys:: order::`.

Red runs: criteria 1 and 3 failed to compile before `Order` existed (`use of undeclared
type Order`), which the spec counts as red. Criterion 4's tests failed on the parent,
which refused every list. Criterion 5's list-form cases pass on the parent for the same
reason, so its red is the mutations; the same test failed on the parent at its "the
bounds themselves are allowed" step. Criterion 2 is a guard.

The mutations were applied one at a time by a script and then reverted. A run stops at
the first suite that fails, so the unit tests killed most of them first; the server and
engine suites were then run alone against the mutations the spec assigns to them. All
were killed. No `cargo mutants` run is claimed for them. "An array's value kept" is not
listed: arrays compare equal by content either way, so it is an equivalent mutant.

1. `several_keys_keep_exactly_a_reference_sorts_prefix`, beside
   `the_second_key_decides_only_what_the_first_ties`: green after. Killed by "only the
   first key compared", "keys compared in reverse order", "a later key's direction taken
   from the first" and "the id tie-break dropped" and "reversed", each in both the kept
   rank and the selector's borrowed comparison (ten mutations).
2. Every existing test passed unedited, in one `cargo test` of the three crates:
   `pstore-query` (32 library tests, 59 integration tests), `pstore-engine` (35 and 536)
   and `pstore-server --test it` (427, the three new ones among them). The server's library tests passed
   but for the two IPv6 tests this container cannot run.
3. `two_keys_order_across_segments_a_delete_vector_and_unfolded_rows`: green after.
   Killed by "only the first key compared" with the engine suite alone.
4. `two_keys_order_by_the_first_then_the_second_then_the_id` and
   `one_key_in_a_list_answers_as_one_pair_does`: green after. The first is killed by "only
   the first key compared" with the server suite alone.
5. `a_list_that_cannot_mean_an_order_is_refused_before_any_blob_request`, which also
   checks the refusals made no blob request: green after. Killed with the server suite
   alone by "the bound is >= 8", "the bound is 9", "a repeated attribute accepted", "a
   key after id accepted", "no keys accepted" and "the list form returns before the
   relevance and window checks". The unit test
   `an_order_is_one_to_eight_distinct_keys_and_none_after_the_id` kills the first five
   too.

Gates, in a cloud container:
- `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings`
  passed.
- ⚠️ Coverage: OBSERVED-NOT locally; llvm-cov in this container drops counts (see
  [M136](../M136/VERIFIED.md)). The floor is CI's run on push.
- NOT-RUN here: `scripts/mutants.sh` (`cargo-mutants` is not installed in this container).
