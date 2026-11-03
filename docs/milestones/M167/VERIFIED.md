# M167 verification

Implemented in `crates/pstore-engine/src/head.rs` (the `exports` section),
`src/replica.rs` (`create_export`, the flag kept at create, set in the finishing commit,
reported and matched) and `crates/pstore-server/src/replication.rs` (the body's
`read_only`, refused on a replication, and the status field). All runs are in a 4-core cloud
container, and every number is `provisional`.

How to run: `cargo test -p pstore-engine --lib exports_round_trip`,
`cargo test -p pstore-engine --test it replicate::` and
`cargo test -p pstore-server --test it replication::an_export`.

Red runs: the engine tests ran against an engine that accepted the flag and did nothing with
it (.harness/m167-red-engine.log). `an_export_is_read_only_in_its_finishing_commit`,
`a_read_only_copy_that_ends_otherwise_says_so` and `a_repeated_export_matches_only_its_flag`
failed. `an_export_is_one_source_epoch` passed: it pins M166's mirror, which already re-reads
the source, and no M167 line can break it. Its mutation, `source_epoch` taken from the
first read, is M166's code; the test's `source_epoch == now > first` would catch it. The
server tests failed against a server that refused the body's field (.harness/m167-red-server.log).
`exports_round_trip_after_copies` failed before the section was encoded.

Suite: 2,638 passed and 0 failed, with the two IPv6 tests skipped because the container has no
IPv6. fmt and clippy pass (.harness/m167-suite.log). The coverage run below ran it again
after M166's CI test was added: 2,639 passed, 0 failed.
Coverage, `scripts/coverage.sh` over the shipping crates: lines 97.32%, regions 95.96%,
functions 97.01% (.harness/m167-coverage.log).

Mutations were applied one at a time by script and reverted. Each ran the test that claims
it (.harness/m167-mutations.log). The script refuses a pattern that does not match exactly
once, and reports a compile error, or a filter that ran no test, as invalid rather than as a
kill. All 15 were killed:
- the section written with a name that has no record, not written, or not read;
- the flag kept for a writable copy, or not cleared by one;
- the flag never applied at the finish;
- the status reporting `false` while running, once ended, or at create;
- `same_copy` ignoring the flag;
- `create_export` making a writable copy;
- the server making an export for `read_only: false`, never making one, accepting the field
  on a replication, or reporting `false`.

The last of these first survived the restore drill, which never looked at the export's flag.
It now checks that the export is read-only. Equivalent and not run: removing the
export entry at a drop or a branch. `encode` drops a name without a record, so those lines
were not added.

No `cargo mutants` run is claimed.

1. PASS: `an_export_is_read_only_until_cleared`: an export answers `read_only` from create to
   success, and its summary says `read_only`. A write, a delete and the drop answer `409
   index_read_only`; a copy without the flag takes a write. After `PATCH` clears the flag a
   write folds, and the status still names the flag asked.
2. PASS: `an_export_is_read_only_in_its_finishing_commit`: a rival write inside the
   interference makes the finishing commit lose once, and before it no HEAD holds the flag
   and the copy reads `running`. After the retried commit, HEAD's epoch is the outcome's
   `epoch` and `read_only` holds `dst`.
3. PASS: `an_export_restores_into_a_fresh_tenant` runs the drill as specified, with server A
   on `main` and server B on `far`. S has two folded segments, a delete vector, integer,
   string and boolean attributes, and a full-text field. The checks: the row digest (ids,
   attributes and vectors) of S, of the export and of R; dense top-k, filter, id and text
   answers against S's; the export read-only; and R writable. R is read by a process without
   `far`, after both S and the export were dropped and collected until their segment objects
   were gone.
4. PASS: `an_export_is_one_source_epoch`: the source compacts, collects, writes and folds
   inside the export's first segment GET. The export succeeds with `source_epoch` equal to
   the source's HEAD after that write, and answers as the source.
5. PASS: `a_read_only_copy_that_ends_otherwise_says_so`: cancelled and failed exports
   report `read_only` and leave no index read-only. A writable copy into the name clears the
   entry and reports `false`. A dropped export leaves no status and no entry.
6. PASS: `a_repeated_export_matches_only_its_flag` (engine) and
   `an_export_is_read_only_until_cleared` (server): the same flag is matched while running
   and after a `PATCH` clear; the other flag is refused (`409` over HTTP).
7. PASS: `exports_round_trip_after_copies` round-trips the section after every earlier one.
   A HEAD whose names have no record encodes byte-for-byte as M166's, and a cut never
   decodes the section.

Code review (`scripts/review.sh context m167`): round 1 failed on one major. AC 2's test never
made the finishing commit lose, though this file said it did. A rival write now makes it
lose. Round 2 passed. A minor is left: the writable copy in
`a_read_only_copy_that_ends_otherwise_says_so` is checked by its status and entry, and is
not synced and written.
