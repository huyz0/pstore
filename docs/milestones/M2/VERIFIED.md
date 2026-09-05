# M2 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md), naming the test or command that
demonstrated it. This **enumerates**; it does not certify the evidence is true beyond what
is recorded here, and a line naming something not actually run would violate the "never
claim a gate ran without running it" non-negotiable.

Gate: `scripts/check-verified.py`.

1. `a_seed_reproduces_the_same_schedule`, `different_seeds_diverge`.
   `cargo test -p pstore-testkit`. The scheduler is SplitMix64 written out in the source
   rather than pulled from a crate, so determinism does not depend on a version bump.
2. `the_auditing_store_rejects_an_unconditional_overwrite`,
   `the_engine_never_mutates_an_object_it_did_not_cas`.
   `cargo test -p pstore-testkit`. The auditing store **refuses** rather than reports:
   Invariant I1 fails the run at the moment it is broken, not in a summary afterwards.
3. `one_hundred_writers_produce_a_dense_epoch_sequence`,
   `every_acknowledged_document_survives_contention`.
   `cargo test -p pstore-engine --test linearizability`. 100 writers; the epoch set is
   gapless and 300 rows are each visible exactly once. Verified sensitive: making commit
   skip an epoch turns the first red with 18 gaps.
   ⚠️ The M1 test `concurrent_committers` no longer demands 32 *distinct* epochs. `fold`
   became tenant-scoped in M2.4, so a fold that finds its work already done reports the
   epoch it found — the collapse is the feature that lets a successor fold a dead lane.
   The property that assertion protected is now asserted directly and more strongly:
   every acknowledged row visible exactly once.
4. `a_writer_paused_past_many_commits_cannot_corrupt`, `a_cas_storm_still_converges`.
   `cargo test -p pstore-engine --test linearizability`. A writer 100 commits behind is
   fenced by CAS alone. Verified sensitive: making `commit` re-read HEAD's tag instead of
   using the observed one lets the stale writer commit.
5. `a_lane_bitmap_records_a_lane_in_one_cas`, `a_successor_finds_the_tail_by_probing_not_listing`,
   `probing_resumes_from_a_watermark_rather_than_from_zero`,
   `a_probe_window_grows_so_a_long_lane_costs_few_round_trips`.
   `cargo test -p pstore-engine --test lanes`. No injected watermark and no LIST: the
   registry is one GET and the tail is parallel probes in growing windows.
   ⚠️ The research this implements was **wrong**. `05-storage-engine/write-path-and-wal.md`
   §3 proposes a bitmap where a lane claims `hash(lane_id) % 65536`; a set bit does not
   name the lane that set it, so it cannot be read back. `lanes.rs` carries the correction.
6. `recovery_finds_every_unfolded_record_across_seeds` (64 seeds, 3 writers, writer death,
   successors on fallback lanes, 15% of writes refused),
   `recovery_after_a_fallback_write_to_another_lane`,
   `a_refused_flush_does_not_punch_a_hole_in_the_lane`.
   `cargo test -p pstore-engine --test oq91`.
   ⚠️ **OQ-91 FAILED on first run**, which is why it was run. Two acknowledged rows were
   lost on seed 0. A refused PUT still consumed the lane's sequence number, and since
   recovery probes forward until a key is missing, a burned sequence is a permanent hole
   — every bundle after it, all acknowledged and all durable, invisible to every future
   reader. One transient failure silently truncated the lane forever. `flush` now advances
   the sequence only after the PUT lands. Both tests were observed red against the old
   code and green against the new.
7. `concurrent_compactors_produce_one_winner` (12 compactors released together),
   `compaction_does_not_change_the_answer`, `compaction_collapses_the_segments_it_merged`,
   `a_fold_landing_mid_compaction_is_not_swallowed_by_it`.
   `cargo test -p pstore-engine --test compaction`. Verified sensitive: reordering the
   merge, dropping the discard condition, and rebuilding from the stale read each turn a
   specific test red.
   ⚠️ `a_losing_compactors_output_is_unreferenced` was **flaky before it was committed** —
   a compactor that read HEAD after the winner published never raced at all. Spawning
   tasks and trusting the scheduler to overlap them is a lottery, so `pstore_testkit::gated`
   holds the first *n* commits at a barrier and releases them together. Ten consecutive
   runs stable.
8. `a_losing_compactors_output_is_unreferenced`.
   `cargo test -p pstore-engine --test compaction`. Both directions: every compactor but
   the winner leaves its output unreferenced, and nothing HEAD names is missing from the
   bucket.
9. `a_dereferenced_object_survives_the_retention_window`, `gc_never_reaps_a_referenced_object`,
   `gc_reaps_a_folded_bundle_once_it_is_beyond_the_window`,
   `gc_prunes_what_it_reaps_so_the_manifest_stays_bounded`, `gc_with_nothing_due_is_free`,
   `gc_refuses_to_reap_a_key_head_still_names`.
   `cargo test -p pstore-engine --test gc`. Retention is counted in **epochs**, not
   seconds, because a reader that read HEAD at epoch *e* may take arbitrarily long to
   finish scanning.
   ⚠️ Mutation testing found GC's live-reference guard was **untested** — removing it left
   every test green. It is unreachable through the normal API by design, so
   `commit_head_for_test` lets a test construct the manifest the commit protocol forbids.
   Removing the guard now turns `gc_refuses_to_reap_a_key_head_still_names` red.
10. Measured, all four:
    * `cargo llvm-cov --workspace --summary-only --ignore-filename-regex pstore-testkit`
      → **95.40% region**, 98.07% line on shipped crates (2738 regions, 126 missed).
    * `cargo llvm-cov --workspace --summary-only --fail-under-lines 95` → 96.49% line,
      the CI gate, exit 0.
    * `cargo mutants --workspace` → 736 mutants, 503 caught, 102 missed, 1 timeout:
      **83.0% workspace, 84.4% on shipped crates.**
    * `./scripts/gates.sh` → all gates green.

    ⚠️ **The mutation score was measuring the wrong thing, twice.** cargo-mutants runs
    only the *mutated package's* tests by default, so a mutant in a `pstore-blob` store
    decorator was never shown the conformance suite in `pstore-testkit` that catches it —
    reported MISSED while a test that fails on it sat two directories away, confirmed by
    applying one by hand. The first fix put the config at the repo root, which
    cargo-mutants does not read; the second run returned byte-identical numbers, which is
    what gave it away. `.cargo/mutants.toml` sets `test_workspace = true`.
    The 68.6% and 72.8% figures measured before that are void, not merely lower.

    ⚠️ Timeouts count as **not caught** above. cargo-mutants' own ratio excludes them
    (83.1% / 84.4%); the stricter reading is recorded because a mutant that hangs is not a
    mutant that was detected. Of the 18 timeouts in the first corrected run, **17 were
    real defects, not slow tests** — see the note under "What this milestone does not
    show".

    ⚠️ Region coverage is reported on **shipped crates**, excluding `pstore-testkit`,
    which is the harness rather than the product. Including it gives 94.81% region. This
    is the M1.12 question and it is still open; the number is stated both ways rather than
    only in the form that passes.

    ⚠️ **68 mutants survive on shipped crates and are not claimed to be covered.** Twenty-
    five of them are in `object_store_backend.rs`, the real S3/GCS/Azure adapter, which
    cannot be exercised without a backend — that is M0a.13, and no amount of local work
    closes it. The rest are mostly PRNG mixing constants in the fault injector, where a
    mutated mixer is still a mixer and the property under test is determinism.

## RA budget

Measured through the request counters, not estimated.

| Operation | Budget | Measured |
|---|---|---|
| Lane registration | 1 CAS per lane lifetime | `a_lane_registration_costs_one_cas_for_the_lane_not_one_per_batch` — 2 writes on the first flush, 1 on every flush after |
| Write batch | 1 W | `a_write_batch_is_one_round_trip_and_one_request` — 1000 rows, one request, depth 1 |
| Tail discovery | *k* parallel probes, 0 LIST | `a_probe_window_grows_so_a_long_lane_costs_few_round_trips` |
| Compaction of *n* | *n* Rpar + 1 W + 1 commit | inputs opened with one `try_join_all`; one `put` before the commit loop |
| GC | manifest diffs, 0 LIST | `gc` reads HEAD's graveyard and issues one `delete_batch` |

## What this milestone does not show

- **Real process death or network partition.** The scenarios model a writer that stops and
  a store that refuses; they do not reproduce a kernel, a socket, or a machine losing
  power. Every OQ-91 result is `provisional` in that sense.
- **That duplicate compaction work is rare.** It shows only that it is *safe*. Suppression
  depends on placement, which is M4.
- **Anything measured on real cloud storage.** Numbers here come from in-process stores on
  WSL2 and are relative, never absolute.

## Two defects found by a test *hanging* rather than failing

Worth recording because the signal was nearly discarded as noise. The first corrected
mutation run produced 18 timeouts, which look like slow tests. Seventeen were mutants whose
behaviour change turned a test into a hang, and both underlying causes were real:

- **`lanes::tail` looped until the store answered 404** — a termination condition owned by
  the remote end. A backend that keeps answering spins forever, on a path a query waits on.
  An unbounded loop whose exit depends on a remote answer is a liveness bug however correct
  its logic. Now bounded, with `EngineError::LaneTooLong`, and tested by
  `a_lane_that_never_ends_is_refused_rather_than_probed_forever`.
- **`Gated` waited at its barrier forever**, so "the racers never arrived" was
  indistinguishable from "still racing". Now bounded and recorded, and every race test
  asserts `raced()` **before** its headline assertion — "exactly one winner" is equally
  satisfied by twelve compactors racing and by one compactor running alone.

After both fixes: 1 timeout, and the score rose from 80.2% to 83.0%.
