# M6d — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a **mutation applied to the shipped code**, the named test run, and
the failure read. Command: `cargo test -p pstore-catalog --test reap`.

1. **A superseded run is reaped and the live one is not** —
   `a_superseded_run_is_reaped_and_the_live_one_is_not`: two folds, `reap(.., 0)` returns 1,
   the first run is gone, the run the head names is present, and `enumerate` still returns
   both records. ⚠️ The live run is never a graveyard entry, so keeping none of the dead ones
   cannot touch it — and if it could, this is where a bucket of tenants vanishes.
2. **The window is counted in folds** — `the_retention_window_is_counted_in_folds`: with
   `retention = 2` a superseded run survives the next two folds and goes on the third.
   Observed red by splitting the newest-first graveyard from the wrong end, which reaps
   exactly the runs the window promised to keep.
3. ⚠️ **Reaping deletes before it commits** —
   `a_reap_that_loses_the_head_cas_still_deleted_and_is_idempotent`. Observed red by moving the
   delete inside the `Landed` arm. **No functional test can see this**: on a store that never
   fails, both orders reap the right objects. Against a backend refusing every CAS,
   delete-then-CAS leaves the graveyard naming absent objects — which the next reap re-deletes
   harmlessly, asserted by a second `reap` returning `Ok(1)` — while CAS-then-delete leaves
   objects with **no record**, and a run's key cannot be derived from anything that survives.
4. **Zero LIST** — `reaping_does_not_list`, over fold, reap and enumerate on `Accounted`.
5. ⚠️ **An append does not erase the graveyard** —
   `an_append_between_a_fold_and_a_reap_keeps_the_graveyard`. Observed red by rebuilding the
   head on the append path (`BucketHead { run_epoch, digest, pending: next, graveyard: vec![] }`)
   instead of mutating the one it read — and **only this test failed**, which is exactly what
   the spec predicted. `Appender::record` keeps the record by accident of style, so the
   accident is now pinned.
6. **A retention wider than the record is refused** —
   `a_retention_wider_than_the_graveyard_is_refused`: `MAX_GRAVEYARD + 1` errors and deletes
   nothing. Observed red by accepting it. ⚠️ A window of 20 against a record of 8 evicts runs
   9 through 20 from the graveyard *while they are still inside the window they were promised*,
   and a run nobody can name is garbage forever. A promise larger than the record is refused
   rather than silently broken.
7. **A pre-M6d head decodes with an empty graveyard** —
   `a_head_written_before_the_graveyard_decodes_and_reaps_nothing`, and
   `heads_round_trip_and_a_truncated_one_is_an_error` now checks the **seam** directly: every
   truncation of a head is an error *except* the one byte offset where the graveyard field
   begins, which decodes as a head without one. That offset is the whole of the additive
   promise, and asserting it inside the existing truncation sweep is cheaper than a fixture.
8. **The graveyard is bounded** — `the_graveyard_is_bounded_by_max_graveyard`, over
   `MAX_GRAVEYARD + 4` folds. Observed red by dropping the truncation. ⚠️ This object is CAS'd
   on **every** append and fold; unbounded, it grows the hottest small object in the catalog
   forever, which is the failure `MAX_PENDING` exists to prevent one field over.
9. **A backend that cannot fence is refused** —
   `a_catalog_write_on_a_divergent_backend_is_refused`: `reap` added to that existing loop over
   every guarded door, asserting `BackendCannotFence` and that the message names the primitive.
   One fixture, not a second.
10. ⚠️ **A width change is conditioned on the root it read** —
    `a_width_change_is_conditioned_on_the_root_it_read` and
    `a_root_write_that_fails_for_io_is_not_reported_as_a_lost_race`.
    **This was not in the delta and is an amendment**: `read_root` returned no tag, so
    `write_root`'s `Some(previous)` arm had no reachable caller and the root was **write-once**
    — the width could be set and never changed, a concrete blocker under OQ-8 one layer below
    its protocol question. Found by the region floor while closing criterion 11.
11. **Gates** — `./scripts/gates.sh` green. `pstore-catalog` **95.10%** regions, 95.65% lines,
    96.41% functions (`cargo llvm-cov -p pstore-catalog --all-features --lib --tests`);
    workspace green via `./scripts/coverage.sh --fail-under-regions 95`.
    ⚠️ 94.85% before criterion 10's tests and **94.93%** after the first of them — the floor is
    what forced the write-once finding out into the open rather than something noticed later.

## What is not built, and named rather than omitted

- ⚠️ **Reaping an orphan run.** A fold that writes its run and loses the head CAS leaves an
  object no head and no graveyard ever names — `a_refused_pointer_cas_leaves_an_orphan_run_and_no_loss`
  pins that half-done fold — and its key cannot be derived from anything that survives.
  Reachable only by **LIST**, which the corpus forbids on read, write and startup paths; a
  reaper is none of those, so a LIST-based sweeper is *permitted* and is a separate decision
  with a cost model. Two cheaper half-measures were considered and rejected: carrying the
  orphan into the retry's graveyard only helps when the writer survives to retry, and a
  tombstone object needs its own reaper. Under contention these accumulate, and this milestone
  does not reduce them.
- **Bucket splitting (OQ-8)** — the other half of backlog item 10, still blocked on a protocol
  keeping an old-width reader **stale rather than wrong**, and on `{bucket:04x}` capping at
  65,536 buckets, which is a key-format change. Criterion 10 removed the write-once blocker
  underneath it; the protocol question is untouched.
- **A caller.** Nothing folds or reaps the catalog on a schedule. `reap` is a mechanism,
  exactly as `fold` is, and the corrected note in M6a's ledger applies here too: the caller is
  whoever composes a serving stack, and nothing does.
