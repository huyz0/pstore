# M200 verification

Implemented in:
- `crates/pstore-engine/src/head.rs`: `CopyProgress`, the `copy_progress` section after the
  ANN sets (its filter, the forced counts, and its refusals), and `commit_at`, which answers
  the tag of the HEAD it wrote.
- `crates/pstore-engine/src/replica.rs`: `mirror` and `rebuild` in batches of
  `copy_checkpoint` segments, `planned`, `tally`, `checkpoint` (only while the record runs
  under its run, at most 3 tries, never an error), the publish planned on the last
  checkpoint's HEAD, the status's `progress`, and the test hook (`Checkpoint`,
  `CheckpointHook`).
- `crates/pstore-engine/src/lib.rs`: `with_copy_checkpoint` (default 64),
  `set_checkpoint_hook_for_test`, and `commit_head_at`.
- `crates/pstore-server/src/replication.rs`: `progress` in `describe_copy`.

The docs are in `docs/api-migrations.md` (M200) and the DELIVERY rows for G's backfill and
H's operation status. All runs were in a 4-core cloud container, so every number is
`provisional`.

How to run:
- `cargo test -p pstore-engine --lib head::`
- `cargo test -p pstore-engine --test it replicate::`
- `cargo test -p pstore-server --lib replication::tests`
- `cargo test -p pstore-server --test it replication::a_running_copy_reports_progress`

**Red first.** The new tests call what the parent does not have (`CopyProgress`,
`with_copy_checkpoint`, the hook, `progress` on the status), so on the parent they do not
compile. The code was then written before each test was seen failing on its behaviour, so
each test is shown to test something by the mutations it kills (below), every one of which
turns it red.

**Suite:** 2,734 passed and 0 failed, with the two IPv6 tests skipped because the container has
no IPv6. fmt and clippy pass (.harness/m200-suite.log). Run without the skips, the same 2,734
pass and exactly those two fail (`ipv6_clients_refuse_malformed_replies_and_release_wire_capacity`
and `serialization_failures_never_send_partial_requests_or_responses`,
.harness/m200-suite-unskipped.log): that is the packet's `cargo test` FAIL. The first suite run failed to compile
the `open` example, which builds a `Head` by its fields; it now names `copy_progress`.

**Coverage:** `scripts/coverage.sh` over the shipping crates gave lines 97.33%, regions 96.01%
and functions 97.04% (.harness/m200-coverage.log).

**Mutations.** I applied 17 mutations one at a time by script and reverted each one
(.harness/m200-mutations.log). The first round left one survivor and two not applied:
- **The run not checked** survived: a cancel removes the record, so only a resume (a new run,
  running again) tells the run check apart. AC 4's test gained that case and kills it.
- **The status's own run filter** could not be killed: a HEAD read from the store has already
  dropped a stale run's entry (`Head::encode` keeps only the record's run). The filter was
  removed, and "the status hides its progress" is killed instead.

All 17 are killed: no checkpoint (copy, and rebuild), the last batch checkpointed, the total
off by one, bytes not added, the publish planned on the stale HEAD, no retry, a fourth try,
the run or `running` not checked, the status hiding its progress, the encode filter dropped or
not checking the run, the ANN counts not forced, a done past its total or a duplicate name
accepted, and the server field unmapped.

**Acceptance criteria.**
1. PASS: `a_copy_reports_its_progress_at_each_checkpoint`. 10 segments, a checkpoint every 3:
   the hook sees `(3, 10)`, `(6, 10)` and `(9, 10)`, each with the copy's run, more bytes than
   the last, and a status that reports it. After the sync the status is `succeeded` with no
   progress, HEAD holds no entry, and the copy answers as its source.
2. PASS: `a_reindex_reports_segments_rebuilt`, the same for a reindex with stemming.
3. PASS: `a_lost_checkpoint_never_fails_a_copy`. Before each checkpoint the tenant takes a
   write, and the checkpoint's first conditional write loses (as to a writer that landed
   between its read and its write): all three are still seen, each on its retry, and the copy
   answers as its source. 3 lost writes in a row at
   the first checkpoint skip exactly that one: `(6, 10)` and `(9, 10)` are seen.
4. PASS: `a_cancel_ends_a_copys_progress`, for a cancel, a pause, and a pause then a resume,
   each at the second checkpoint. Every later checkpoint writes nothing, and HEAD's epoch right
   after it is the cancel's, the pause's or the resume's own. The batches still run to the
   publish's plan check, which drops them. A cancelled copy reports no progress, a paused one
   `(3, 10)`, and a resumed one runs under a new run with no progress. An `epoch_unavailable`
   copy is an outcome, and an outcome reports no progress by construction (`copy_status_in`).
5. PASS: `copy_progress_round_trips_after_ann`. Over three base HEADs it checks:
   - the section round-trips alone and after the other sections;
   - a name `once` lacks, a run that differs, and a replication not in `once` each encode as
     the HEAD without it;
   - a done past its total, a name twice and a trailing byte are refused;
   - no cut decodes the section.
6. PASS: `a_copy_reports_its_progress_at_each_checkpoint`: the same 10-segment copy costs exactly 6 requests more with a
   checkpoint every 3 than with every 64: 3 checkpoints, a GET and a PUT each, and no lost
   publish. The existing replication cost tests pass unchanged.
7. PASS: `a_running_copy_reports_progress` (`null` while running before a checkpoint and once
   succeeded) and `progress_is_described` (each field by its name, `updated_at` as the other
   times).
8. PASS for `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, the suite and
   `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95`, run here (see above).
   `scripts/mutants.sh` on CI's 6 shards first missed two mutants: `>` to `>=` in the
   progress decoder (a done equal to its total was never round-tripped) and the update time deleted
   from the checkpoint's entry (its time was never asserted). The HEAD unit test now
   round-trips a done equal to its total, and AC 1's `consistent` asserts each checkpoint's
   time is at or after the copy's creation. Each mutation was applied here and turned its test
   red, and each clean tree passes. The re-run is CI's, on the PR.

Code review (packet `.harness/review/m200.r1.packet`) passed with no blocker or major. Minors:
- Taken: AC 3's first case wrote before the checkpoint's read, so no try lost. Each first try
  now loses, and the "no retry" mutation is killed by that case too.
- Left as they are: a segment that keeps reading as missing while still in the source is
  planned again on each remap, so `segments_total` can over-count (done stays at or below it);
  after a stop, the next attempt is planned on the last checkpoint's HEAD and loses one write
  before a fresh read; and the server test shows `progress` only as `null`, since a copy there
  has fewer than 64 segments (the field's mapping is the unit test's).
