# M168 verification

Implemented in `crates/pstore-engine/src/head.rs` (`Reindex` and its `apply`, the `reindexes`
section, the `rebuild_refused` code), `src/replica.rs` (`create_reindex`, the create-time
refusals, `rebuild` and `rebuild_segment` in place of the byte copy, the refused seal ending
the copy, the repeated-POST match) and `crates/pstore-server/src/lib.rs` and
`src/replication.rs` (the body's `schema`, read by the write's option parser, refused on a
replication, and the new code). The support matrix is in `docs/api-migrations.md`. All runs
are in a 4-core cloud container, and every number is `provisional`.

How to run: `cargo test -p pstore-engine --lib reindex`,
`cargo test -p pstore-engine --test it replicate::a_re` and
`cargo test -p pstore-server --test it replication::a_reindex`.

Order of work, stated because it is not the order the `tdd` skill asks for. The HEAD test
was written first and seen red against an encoder that did not write the section
(.harness/m168-red-head.log); `a_reindex_replaces_what_it_names` was written after `apply` and
passed on its first run. The engine and server tests were written before the code they
exercise, but they could not compile until its API existed, so they were first run against
the finished code. Each was then seen red by the mutations below, which break the line it
covers. The server test caught a real defect on its first run: a dropped further field was
read back as the primary text field and kept its postings.

Suite: 2,651 passed and 0 failed, with the two IPv6 tests skipped because the container has no
IPv6. fmt and clippy pass (.harness/m168-suite.log).

Coverage, `scripts/coverage.sh` over the shipping crates, which ran the suite again (2,651 passed, 0
failed): lines 97.33%, regions 95.98%, functions 96.99% (.harness/m168-coverage.log).

Mutations were applied one at a time by script and reverted (.harness/m168-mutations.log).
Each ran the test that claims it. The script refuses a pattern that does not match exactly
once, and reports a compile error, or a filter that ran no test, as invalid rather than as a
kill. All 38 were killed. The first run had four survivors:
- the regex set merged with the source's rather than replaced;
- the source's dictionaries kept on a rebuilt ref;
- the source's further fields allowed as the primary;
- the fallback primary not excluding a further field.

All four were killed by the new `a_reindex_drops_what_it_leaves_out`. The last of them was
equivalent until a redundant trailing filter was removed. The mutations, in groups:
- the section: written for names without a record, not written, or decoding the new code as
  another;
- `apply`: the further fields merged, the regex set merged, or the primary's options kept;
- the rebuild: a byte copy instead, the hash without the delete vector, deleted rows kept, an
  all-deleted segment sealed, the source's row count, dictionaries or length kept, or the
  seal's names not buried;
- a refused seal: ended as a store failure, a store failure ended as refused, or a refusal
  retried;
- the create-time refusals: each one;
- the asked options: not matched, not recorded, not cleared by a plain copy, not reported
  (while running, once ended, at create), or not sorted;
- the server: the code's name, `schema` accepted on a replication, the options dropped, the
  primary's options or the regex set dropped, and the regex type, pre-tokenized and field-count
  checks.

No `cargo mutants` run is claimed.

1. PASS: `a_reindex_rebuilds_under_a_new_analyzer`, from another tenant and from another store.
   After one sync the copy is `succeeded`, `dest`'s schema records stemming, and "run" finds
   "running" on `dest` and nothing on the source. The source's schema and segment list are
   unchanged, and the row digests are equal.
2. PASS: server `a_reindex_adds_and_drops_text_and_regex_fields` (adding `body`, `tag`'s regex,
   and the primary's stemming; dropping `title`). A `body` query answers. A `title` query is
   `unknown_field`. The summary reports `body`, `["tag"]` and stemming, and a `Regex` filter on
   `tag` answers. A write declaring the source's set is `400 schema_conflict`, and one
   declaring the new set folds and answers. The engine's `a_reindex_drops_what_it_leaves_out`
   drops the only further field and the regex set: no text dictionary is written, the ref says
   so, and the dropped field finds nothing.
3. PASS: `a_reindex_keeps_live_rows_and_no_deletes`. Three segments: one with deletes, one
   whose rows are all deleted, and one with none. `dest` has the source's 48 live rows (by
   digest) in two segments and no delete vector. No segment's `h` is a byte copy's. Each ref's
   `len` and dictionaries are its own object's. The dense, filtered and sparse answers and an
   id lookup equal the source's.
4. PASS: `a_reindex_is_one_source_epoch_under_deletes`. The trigger sits in the other segment's
   GET rather than the deleted one's, as the spec words it; the deleted segment is rebuilt
   without the delete either way. Inside the other segment's first GET,
   the source deletes `d0001` and folds, and that GET then answers 404 once. The reindex
   succeeds at the source's HEAD after the delete, with 59 rows and no `d0001`.
5. PASS: `a_reindex_never_reads_as_finished_partway`. Inside the interference, the status is
   `running` with its options, and no HEAD names `dst`'s segments or schema. The cancel of a
   second reindex, `dst2`, is the rival commit that makes the finish lose once. `dst`
   succeeds at the HEAD it committed, and `dst2` is `cancelled` with no index. `dst2`'s rebuilt
   objects are in the graveyard, and after a GC neither index has leaked anything.
6. PASS: `a_refused_rebuild_fails_typed`. A further text field named past `INDEX_BUDGET`
   (the source folds with it as an ordinary attribute) ends the copy `failed` with
   `rebuild_refused` after one sync, with no index. A second sync is idle and reads no source
   segment. A seal whose first create fails on the store leaves the copy `running` with a
   failure reported, and the next sync finishes it. The server test reports the code as
   `rebuild_refused`.
7. PASS: `a_repeated_reindex_matches_only_its_schema` (engine) and AC 2's server test. The same
   options are matched while running and after success. Other options, none, or a plain copy
   are refused (`409` over HTTP). A regex set spelled in another order is the same ask. A
   plain copy after a cancelled reindex reports no options.
8. PASS: `a_reindex_body_is_checked` (server) and `a_reindex_naming_the_primary_is_refused`
   (engine). The server checks each body before it reads the source: a body that passed its
   checks would answer 404, because the source does not exist. The engine refuses the primary,
   `id`, a `uint64` attribute as a text field or regex attribute, and nine further fields. A
   source segment read would fail the test, and no copy is left behind.
9. PASS: `reindexes_round_trip_after_exports` round-trips the section, alone and after the
   exports, and the new failure code. A HEAD whose names have no record encodes byte-for-byte
   as M167's, and a cut never decodes the section. Of the gates it names, fmt, clippy, the
   suite and coverage ran here (above); `cargo mutants` is left to CI's mutation shards.

Code review (`scripts/review.sh context m168`) passed in round 1. Two minors are left. A
source segment this build cannot open or scan is reported and retried each sync, as M166's
byte copy does, rather than ending the copy `rebuild_refused`, which the spec limits to the
seal. A source whose every row is deleted (`dest` then has a schema and no segment) is not
tested.
