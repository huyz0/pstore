# M177 verification

The spec was reviewed in two rounds and amended once in implementation, all recorded in the
[spec](SPEC.md). Code review round 1 passed with three minors, recorded in the commit body.

How to run the tests:
- format: `cargo test -p pstore-format --test it -- sparse_keys:: sparse:: footprint::`;
- index: `cargo test -p pstore-index --test it -- sparse::`;
- engine: `cargo test -p pstore-engine --test it -- sparse:: split::`;
- peer wire: `cargo test -p pstore-server --lib -- peers::tests::a_part_survives`.

Red run: the new tests do not compile against `u32` dimensions, so their red is the
mutations below, as M176's unit tests' was: applied one at a time by a script against the
four commands above (`split::` aside) and reverted, all 19 killed; after M178's spec review
widened the dictionary's key offsets to `u64`, the nine on `sparse.rs` ran again, all killed.
No `cargo mutants` run is claimed for them.

1. `sparse_keys::every_key_round_trips_and_keeps_its_own_postings` (70,010 keys, as sorted
   sets, through a bundle, a scan and a rebuild of the scan, each key's list exact) and
   `sparse::string_keys_survive_the_bundle_the_fold_and_the_compaction_each_their_own` (the
   adversarial keys through the engine's flush, fold and compaction): green. Killed: a key
   cut to 8 bytes when built, or to 4 in a bundle; keys hashed into one; a key's length
   written in a byte; a scan naming the previous entry's key.
2. `sparse::string_keys_rank_exactly_like_an_exhaustive_dot_product` (2,000 rows, F32, the
   adversarial keys but the 65,535-byte one and 40 more, dyadic weights so ties are exact):
   green. Killed: a repeated query key keeping its first weight; the binary search
   comparing reversed.
3. `sparse::two_writers_naming_one_key_write_one_dimension`: green. The peer clause:
   `peers::tests::a_part_survives_the_wire_bit_for_bit` now carries adversarial keys, and
   `split::a_split_query_equals_the_unsplit_one` (its sparse case, unchanged but for decimal keys) is green.
   Killed: the peer leg's keys dropped.
4. `sparse_keys::a_u32_dimension_written_before_m177_reads_as_its_spelling_and_only_that`,
   `a_compaction_of_old_and_new_segments_writes_the_new_dictionary`,
   `what_no_version_wrote_is_refused` and
   `a_run_holding_string_keys_is_refused_by_an_older_reader_and_no_other_is`: green. The
   older reader is a copy of its version check only, over two runs encoded apart; a bundle
   decodes each index's run on its own (`bundle.rs`). Killed: v1 refused; v1 left in numeric
   order; v1 matched by parsing; version 3 for a run with no sparse field; kind 2 taken in a
   version-2 run; kind 1 read as kind 2.
5. `sparse_keys::a_repeated_empty_or_overlong_key_is_refused_at_the_door` and
   `footprint::a_document_counts_its_id_vectors_and_attributes` (its sparse case changed to
   the new rule, keys of two lengths): green. Killed: each refusal removed; the footprint
   counting 8 a pair. The existing sparse depth and byte tests in `pstore-index`'s
   `sparse::` pass unchanged. Changed existing tests: fixtures now key dimensions by their
   spelling, and five that named dimension 3 twice (`i % 7 == 3`), now refused, name 7.
   The workspace suite, without the two IPv6 tests, rebased on main after M176 merged
   (2d80ca62): 2,547 passed, none failed. `scripts/coverage.sh --changed --fail-under-lines 95 --fail-under-regions 95
   --ignore-run-fail` with those skips: lines 97.45%, regions 96.08% (`provisional`: a
   4-core cloud container). CI's coverage and mutants runs on the PR are the gate.
