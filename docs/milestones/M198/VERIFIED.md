# M198 verification

The spec was reviewed in one round, recorded in the [spec](SPEC.md), and amended while the
code was written and after code review (recorded there). Code review is recorded in the spec
and the commit body.

How to run the tests: `cargo test -p pstore-engine --lib -- head::`,
`cargo test -p pstore-engine --test it -- ann_fields:: replicate::a_replica_records
replicate::a_reindex_keeps` and `cargo test -p pstore-server --test it -- vector_search::`.

Red run: on the parent, with the tests and none of the source, the engine's unit and
integration tests do not compile (they read `SegmentRef::ann`, which the parent does not
have: 14 errors), and the server's tests read a `vector_search` the parent's summary does not
answer. Then each mutation below, applied one at a time by a script and reverted: 23 killed,
none survived. One was first unviable as written (the duplicate-index check removed moved a
value) and was rewritten to a viable form, which is killed. After code review changed the
decoder and the counts, the 12 mutations in HEAD's code were run again: all killed. No
`cargo mutants` run is claimed.

1. `ann_fields::heads_sets_agree_with_what_a_query_searches_by_clusters` (engine, three
   segments of 5,000, 5,000 and 200 rows with `vector`'s threshold lowered to 1,000, 30 rows
   unfolded; for `vector`, `a` and `b` the segments HEAD names each in equal a default query's
   `explain` approximate and exact segment counts, (2, 1), (2, 1) and (0, 3); the same from a
   fresh engine, after a branch, and after a compaction),
   `vector_search::each_dense_fields_rows_are_answered_by_how_a_query_searches_them`,
   `vector_search::a_field_a_segment_searches_by_clusters_is_answered_approximate` and
   `vector_search::an_index_with_no_vector_answers_no_dense_field`: green. Killed: `vector`
   recorded whatever the centroid table, named fields recorded whatever the set was written,
   multi-vector names recorded, a compaction's set left empty, the fold's set unknown,
   `exact_rows` and `unknown_rows` swapped, approximate rows counted exact, `vector`
   answered for an index of width 0, and rows counted with their deleted ones.
2. `head::tests::the_ann_section_round_trips_and_refuses_malformed_input`,
   `replicate::a_replica_records_its_sources_dictionaries` and
   `replicate::a_reindex_keeps_live_rows_and_no_deletes`: green. Killed: the count off by one
   both ways (decoded and encoded), unknown read as empty, the table not checked against the
   refs' names, the table count off, a duplicate index admitted, a field twice in a ref
   admitted, an index whose sets are all empty given no entry, the section never written,
   the earlier sections it forces left unforced (two), an ordinal off by one, a replica's
   sync dropping the source's sets, and a replica's rebuild recording an empty set.
3. `patch::a_plain_fold_reads_what_it_did_before_m13` and
   `served_epoch::a_read_only_process_reports_the_epoch_it_served`: green with their HEAD
   byte pins raised by the section's 43 bytes, counted in their comments (the seven forced
   zero counts, a count, the name, three one-byte varints). Their read counts are unchanged,
   as are every cost and depth test in the suite, M195's pins among them.
   `scripts/index-scale.sh` (built without LTO: the release LTO link of the server tests was
   killed for memory in this container): green, HEAD 249, 239 and 239 bytes an index at 50,
   1,000 and 5,000 indexes against its 256, with query and fold request counts unchanged
   (`provisional`: a 4-core cloud container).

Gates on the final tree: `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`
clean. The workspace suite, without the two IPv6 tests: 2,723 passed, none failed.
`scripts/coverage.sh --changed --fail-under-lines 95 --fail-under-regions 95
--ignore-run-fail` with those skips, after `cargo llvm-cov clean`: lines 97.24%, regions
96.25% (`provisional`: a 4-core cloud container). CI's coverage and mutants runs on the PR
are the gate.
