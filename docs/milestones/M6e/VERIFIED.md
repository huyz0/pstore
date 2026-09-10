# M6e — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a **mutation applied to the shipped code**, the named test run, and
the failure read. Command: `cargo test -p pstore-catalog --test sweep`.

1. **An orphan is swept and the live run is not** —
   `an_orphan_run_is_swept_and_the_live_one_is_not`: three folds, an object at a run key
   nothing names, `sweep` returns 1, the head's own run survives and `enumerate` still returns
   every record.
2. ⚠️ **A run in flight is never touched** — `a_run_in_flight_survives_the_sweep`, and this is
   the milestone. **Observed red by comparing on `<=` instead of `<`**, which deletes the
   object a winning CAS is about to publish — after which the head names an object that does
   not exist and every enumeration of that bucket is `MissingRun`. A bucket's worth of tenants,
   traded for one piece of garbage. The same test also pins the **same-epoch** CAS loser, whose
   epoch equals the head's rather than being less: collected by the next sweep, never this one.
3. ⚠️ **The graveyard's runs survive** — `the_graveyards_runs_survive_the_sweep`. Observed red
   by not consulting the graveyard. Those runs are unnamed by the head and below its epoch —
   the sweeper's own definition of garbage — so ignoring the graveyard reaps exactly the runs
   M6d's retention window promised a mid-enumeration reader.
4. ⚠️ **Anything unrecognised survives** — `an_unrecognised_object_under_the_prefix_survives`,
   asserted with the bucket's `HEAD` **and** a foreign object under the same prefix. Observed
   red by flipping the default to "delete what does not parse". Special-casing `HEAD` by name
   would pass half of this and still delete everything a later milestone puts beside it.
   `run_key_round_trips_through_parse` pins the other half: `parse_run_key` is the inverse of
   `run_key`, refuses another bucket's key, and refuses four near-miss shapes.
5. **It records nothing and is idempotent** — `a_second_sweep_finds_nothing_and_changes_nothing`:
   the second sweep returns 0 and the head object is byte-identical. An orphan is defined by
   *absence* from the head, so there is nothing to write down — which is also what makes this
   safe beside a concurrent fold.
6. ⚠️ **A backend that cannot fence is refused** — `a_sweep_on_a_divergent_backend_is_refused`
   asserts `BackendCannotFence` **and zero deletes**, and `sweep` is added to `refusal.rs`'s
   existing loop over every guarded door.
7. **Gates** — `./scripts/gates.sh` green; workspace regions green via
   `./scripts/coverage.sh --fail-under-regions 95`.
   ⚠️ **`pstore-catalog` itself measures 94.90% regions, below the 95% this criterion states,
   and it is reported rather than rounded.** The gap is `append.rs` at 88.75%, and it is the
   mutex **poison** branches in the appender's observe path, which degrade to "record
   unconditionally" rather than panicking. Reaching them needs a thread that panics while
   holding a private lock. Backlog row rather than a contrived test, and the criterion is
   left as written rather than moved to match the result.

## What is not built, and named rather than omitted

- ⚠️ **The cost of the LIST, per bucket.** A deployment-wide sweep at `DEFAULT_WIDTH` is
  **16,384 LISTs**, each priced like a PUT. That scales with buckets and never with tenants —
  1M tenants does not change it — but nothing bounds how often it runs, because nothing runs it.
- **Pagination.** `list_unrestricted` returns a `Vec<Key>` with no cursor and at most 1000
  keys, so a bucket holding more than that is swept across several runs: **incomplete, not
  wrong**. Nothing reports how far behind it is.
- ⚠️ **A bucket with no head sweeps nothing** — `a_bucket_with_no_head_sweeps_nothing`. An
  absent pointer reads as `Epoch::ZERO` and nothing is strictly below zero, so a first fold
  that lost its create-if-absent leaves an orphan uncollectable until some fold succeeds. Safe,
  and pinned rather than discovered.
- ⚠️ **The orphan rate is still not measured.** M6d said these accumulate under contention;
  this collects them without saying how many there were. `sweep`'s return count is the only
  signal and nobody reads it.
- **A schedule.** Nothing calls `fold`, `reap` or `sweep` on a timer. The caller is whoever
  composes a serving stack, and nothing does.
