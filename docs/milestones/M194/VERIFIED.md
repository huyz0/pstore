# M194 verification

Implemented in these files:
- `crates/pstore-engine/src/head.rs`: the `copy_times` section, its filter, and the forced
  counts.
- `crates/pstore-engine/src/replica.rs`:
  - the create's record, written in the `once` branch of `create_inner`;
  - the `POST` answer's time, captured from the winning attempt;
  - `stamp_end`, called at the cancel, at the finish, and at the ended (`gone`) copies;
  - the status.
- `crates/pstore-server/src/replication.rs`: `created_at` and `ended_at`.

The docs are in `docs/api-migrations.md`. All runs were in a 4-core cloud container, so every
number is `provisional`.

How to run:
- `cargo test -p pstore-engine --lib head::`
- `cargo test -p pstore-engine --test it replicate::`
- `cargo test -p pstore-server --test it replication::a_copy_status_has`

**Red first.** Each test was written before the code it checks and was seen red:
- The HEAD tests failed against a field with no encoding (.harness/m194-red-head.log).
- The engine tests failed against status fields that were always `None`
  (.harness/m194-red-engine.log).
- The server test failed against a status without the two fields
  (.harness/m194-red-server.log).

**Suite:** 2,681 passed and 0 failed. The two IPv6 tests were skipped because the container
has no IPv6. fmt and clippy pass (.harness/m194-suite.log).

**Coverage:** `scripts/coverage.sh` over the shipping crates gave lines 97.33%, regions 95.97%
and functions 97.02% (.harness/m194-coverage.log).

**Mutations.** I applied 18 mutations one at a time by script and reverted each one
(.harness/m194-mutations.log). Each ran the tests that claim it. The first run had three
survivors, and each was resolved:
- **The not-ended filter on `ended_ms` removed.** It survived because an ended copy's record
  is always stamped and never 0. The filter was dead, so I removed it.
- **The create's time written for a replication too.** It survived because the encode filter
  drops the record, as the spec says. I moved the write into the existing `once` branch of
  `create_inner`. Making that branch unconditional breaks 28 replication tests.
- **The decoder's new end check removed.** It is killed by
  `head::reindexes_round_trip_after_exports`, which the first run's test filter did not
  include.

The other 15 were killed:
- the create's record not written;
- the answer's time not captured;
- the running status's time dropped;
- the not-ended filter inverted;
- each of the three `stamp_end` calls removed;
- the clock-0 guard removed;
- an end inserting a record where none was;
- the encode filter dropped;
- each of the two forced counts not forced;
- created and ended swapped in decode;
- each server field dropped.

No `cargo mutants` run is claimed. CI's mutation shards are the check.

1. PASS: `a_copy_reports_when_it_was_created_and_ended`. It checks:
   - Created at 1,000, the `POST` answer and the status both report `(Some(1000), None)`, and
     still do after a pause at 2,000 and a resume at 3,000.
   - The sync at 5,000 ends it `succeeded` with `(Some(1000), Some(5000))`.
   - A repeated `POST` at 6,000 answers the same pair.
2. PASS: `every_copy_end_is_stamped`. Each case keeps its own `created_ms`.
   - A cancel at 7,000.
   - A refused reindex synced at 8,000, ending `rebuild_refused`.
   - A copy whose source was dropped after the create, synced at 9,000, ending
     `source_not_found`.
   - A copy whose record was removed from HEAD and is then cancelled: it reports `(None, None)`
     and HEAD holds no record afterwards.
   - A cancel with the clock at 0: it reports `ended_ms = Some(1)`.
3. PASS: `a_copy_time_never_outlives_its_copy` (engine) and
   `evicted_copy_times_are_not_encoded` (unit).
   - A new copy into the name of a failed copy reports its own `(Some(3000), None)`.
   - A replication into such a name leaves no record in HEAD as read back.
   - Of 65 recorded copies, the one `record_copy` evicted has no record after a round trip,
     and 64 records remain.
4. PASS: `copy_times_round_trip_after_reindexes`. It checks:
   - The section round-trips alone and after the exports and reindexes, over three base HEADs.
   - A name with no record, or a `once` name whose replication is gone, encodes as M168's
     bytes.
   - A trailing byte is refused, and no cut decodes the section.
5. PASS: `a_copy_status_has_wall_clock_times`. It checks:
   - The `POST` answer's `created_at` is an RFC 3339 millisecond time inside the wall-clock
     bounds of the request.
   - `ended_at` is `null` while the copy runs.
   - The status repeats `created_at`.
   - After the sync, `ended_at` is a time between `created_at` and the read.
6. PASS: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, the suite and
   `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` ran here (see above).
   `scripts/mutants.sh` is left to CI's shards.


Code review: the review packet was prepared (`scripts/review.sh context m194`), and the review is
pending. This line is replaced by its outcome before merge.
