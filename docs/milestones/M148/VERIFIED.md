# M148 verification

Implemented in `crates/pstore-engine/src/head.rs` (the `times` section after M152's
further text fields, and the existence filter in `encode`), `crates/pstore-engine/src/lib.rs`
(`stamp_times` and its three callers: create, branch and fold; the test clock; and
`IndexStats`' `created_ms`, `written_ms` and `bytes`), `crates/pstore-engine/src/replica.rs`
(the sync's stamp) and `crates/pstore-server/src/lib.rs` (the summary's `created_at`,
`updated_at` and `approx_logical_bytes`, and `rfc3339_ms`). All runs are in a 4-core cloud
container, and every number is `provisional`.

How to run: `cargo test -p pstore-engine --lib head::`,
`cargo test -p pstore-engine --test it times`, `cargo test -p pstore-server --test it times::`
and `cargo test -p pstore-server --lib times_are`.

Red runs. `times_round_trip_beside_keys_since` was written before the encoding and failed
against it (.harness/m148-red-head.log). `times_are_rfc3339_at_millisecond_precision` was
written against the first formatter, which dropped a zero fraction, and failed
(.harness/m148-red-rfc3339.log). The engine, replica and server tests were written after
the code they call existed, so their red is the mutations below.

Mutations, applied one at a time by script and reverted, each running the head unit tests
and the engine and server `times` tests (.harness/m148-mutations.log,
.harness/m148-mutations-r2.log, .harness/m148-mutations-r3.log). 20 of 21 were killed:
each of the four stamp sites removed (create, branch, fold, sync); the fold stamping
without its rejects; `written` not taking the max; `created` always now; existence
without rejects or without quarantine; `encode` without its filter, or filtering without
rejects or quarantine; times not forcing the sections before them; `created_ms` 0 not
filtered to `None`; bytes 0 for an unknown length, or summing only the first segment; the
unfolded summary's bytes `None`; and each summary field dropped. The first sweep left five
alive, and `a_fold_of_only_rejected_rows_writes_its_index`,
`an_index_held_only_by_rejects_or_quarantine_already_existed` and
`an_unfolded_index_reports_no_bytes_and_no_times` were added for them.

⚠️ The second sweep's server step did not compile (a build directory shared with another
worktree), and its script counted the compile error as a kill. Code review caught it. The
script now reports a compile error as invalid, and the two mutants whose kill rested on
that step were rerun with a clean build (r3). One was killed. The other, `created` taken
from the previous time only when the name existed, survives and is equivalent: every
HEAD a commit reads is decoded, and `encode` writes no time for a name that does not
exist, so a previous time is never present for a name that did not exist.
No `cargo mutants` run is claimed.

1. PASS: `summary_reports_times_and_bytes`. After a create over HTTP, `created_at` lies
   within the test's clock window around the create, `updated_at` equals it, and
   `approx_logical_bytes` is 0.
2. PASS: `fold_sets_written_time_only_for_its_indexes`, `compaction_sets_no_time`,
   `a_fold_of_only_rejected_rows_writes_its_index`. A fold moves `written_ms` and keeps
   `created_ms`, a delete and a fold of only rejected rows are writes, a compaction moves
   neither, and a fold of another index moves neither.
3. PASS: `first_fold_creates_the_time`, `an_index_from_before_m148_has_no_created_time`,
   `an_index_held_only_by_rejects_or_quarantine_already_existed`, `branch_and_drop_times`,
   `times_never_outlive_their_index`, `a_duplicate_key_moves_no_time`,
   `a_sync_sets_the_replicas_times`.
   - A first fold sets both times. An index from before M148, including one that exists
     only by rejects or a quarantine, gets `written_ms` and a `None` created time.
   - A branch's destination gets its own times and leaves the source's. A drop leaves no
     entry, and a re-creation gets new times.
   - A delete-only fold of an absent name leaves no entry, and the same process's later
     fold of that name creates it then. Discarding the quarantine of an index that lived
     only by it removes its entry.
   - A duplicate idempotency key's retry moves nothing.
   - A replication's create sets nothing. The sync that creates the replica sets both, a
     synced source write and a synced source compaction each move `written_ms`, and an
     idle sync moves nothing.
4. PASS: `summary_reports_times_and_bytes`, `bytes_are_unknown_when_any_segment_length_is`.
   After two folds, `approx_logical_bytes` equals the sum of the two segment objects'
   lengths, read back from the store by `head`. With one segment's recorded length
   removed, the engine's `bytes` is `None`.
5. PASS: `a_slow_clock_never_moves_a_time_back`. A process at 4,000 ms folding after one
   at 9,000 ms leaves both times at 9,000.
6. PASS: `times_round_trip_beside_keys_since`. Each combination of absent and present
   `keys_since` and outcomes round-trips with times. A HEAD whose times name only absent
   indexes encodes byte for byte as one without times. A trailing byte is refused, and
   no truncation decodes times.
7. PASS: `summary_reports_times_and_bytes` and
   `an_unfolded_index_reports_no_bytes_and_no_times` assert `blob_reads` 1 for every
   summary they read. Two pinned byte counts grow by the section's 56 bytes for one index,
   itemised in `patch.rs` and `served_epoch.rs`; their read counts are unchanged.
8. PASS, with the IPv6 exception M127 records.
   - `cargo clippy --all-targets --all-features -- -D warnings` passed.
   - The workspace suite passed 2,355 tests with none failing
     (.harness/m148-quality-suite.log). It ran with all features and both `[::1]` tests
     skipped: the container has no IPv6, and those tests run in CI.
   - `./scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` passed at
     97.20% lines and 95.78% regions (.harness/m148-coverage.log).
   - `scripts/recall.sh`, `scripts/ndcg.sh` and `scripts/depth.sh` passed
     (.harness/m148-quality.log).

Rebased onto main after M152, which had appended its further text fields after
`keys_since`. `times` now follows them, forcing their count (0 when none) and so
`keys_since` (`u64::MAX` for none, as M152 writes it); the marker form and M147's 8-byte
test are gone. `times_round_trip_beside_keys_since` gained a round trip beside M152's
fields, live and dropped, and fails with `times` not forcing their count
(.harness/m148-rebase-mutation.log). M152's byte-for-byte test still passes, so a HEAD
without times encodes as before. The two pinned counts grew by 11 bytes each, the forced
`keys_since` and count in place of the marker.

Code review: see the commit message.

Rebased again after M173, which had appended its recorded types after the further text
fields. `times` now follows the types and forces their count (0 when none), which forces
everything before it. A HEAD without times still encodes as before, and M173's own tests
pass unchanged. The two pinned counts grew by 4 bytes each, the types' forced count
(.harness/m148-m173-suite.log). `times_round_trip_beside_keys_since` gained a round trip
beside recorded types, live and dropped, and fails with the times not forcing the types'
count and with the times written before the types (.harness/m148-m173-mutations.log).
