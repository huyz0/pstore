# M118 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, 4 cores, debug profile. Every count
below is from here; macOS and Windows ran none of the stress runs.

⚠️ **The deadlock, seen.** gdb on a hung run of criterion 3 without `with_spawner`: a thread
named `tokio-rt-worker` -- the test's own runtime -- in
`OwnedTasks::close_and_shutdown_all` → `ReclaimingBlock::drop` → `on_reclaim_finish` →
`reclaim_if_needed` → `Spawner::spawn` → `OwnedTasks::bind` (closed, so the task is shut down
at once) → `ReclaimingBlock::drop` → `on_reclaim_finish` → `RwLock::write_contended`. One
thread, one lock, taken twice. foyer-storage 0.22.3, as `Cargo.lock` pins.

⚠️ **A first criterion 3 was dropped.** Driving the burst through store reads, 400 drops at 40
delays hung 0 times before the change and 0 after: it could not tell the two apart. Inserting
into the tier directly hung 25 of 100 drops before the change.

1. `disk::tests::foyer_runs_on_the_tiers_workers` passes. **Red first** with `with_spawner`
   removed: `left: Some("disk::tests::foyer_runs_on_the_tiers_workers")`,
   `right: Some("pstore-cache")`.
2. `disk::tests::a_dropped_tier_stops_its_workers` passes. **Red first** two ways: with
   `Workers::drop` emptied, it panicked in tokio's `blocking/shutdown.rs` (a runtime dropped
   inside an async context); with the runtime passed to `mem::forget` instead of shut down,
   `a task ran on a dropped tier's runtime`.
3. `disk::tests::a_runtime_dropped_mid_flood_returns` passed 5 of 5, each in about 0.6 s.
   **Red first** with `with_spawner` removed: failed 5 of 5,
   `a runtime dropped with the tier open never returned`, each after its 20 s wait.
4. By hand, Linux: a scratch test opening, closing and dropping 20 cores counted
   `/proc/self/task` at 2 threads before and 2 after. With `Spawner::Runtime` (a runtime
   handed to `foyer` whole) the same count went from 2 to 208. The scratch test is not kept:
   the count is the process's, and the suite runs tests in parallel. NOT-RUN on macOS and
   Windows.
5. `./scripts/gates.sh` passed, run by the pre-commit hook on this commit. Mutation:
   `./scripts/mutants.sh --check disk --in-diff <the staged diff> --test-workspace=false
   --test-package pstore-cache` found 3 mutants: 1 caught, 2 unviable, none missed.
