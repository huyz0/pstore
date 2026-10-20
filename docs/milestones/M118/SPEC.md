# M118 — the disk tier's tasks run on its own runtime

**Serves:** the self-deadlock M115's ledger reports and leaves open: a runtime shut down with
the disk tier open could hang the thread shutting it down. No backlog row: a defect.

## What is true today

- **`foyer` spawns under its own lock.** `BlockManager::reclaim_if_needed` spawns a reclaim
  while it holds the block state's write lock. A runtime that is shutting down drops a task
  it is handed at once, and a reclaim's `ReclaimingBlock` takes that lock again when dropped:
  `std`'s `RwLock` is not reentrant, so the thread waits on itself forever. A dropped
  in-flight reclaim reaches the same spawn, so any shutdown with a reclaim in flight and a
  flusher waiting for a clean block hangs.
- ⚠️ **Drift in M115's ledger.** It names foyer-storage 0.22.6. `Cargo.lock` pins 0.22.3. Both
  hold the same code here (`reclaim_if_needed`, `on_reclaim_finish`), so the finding stands,
  for both.
- **The runtime is the caller's.** The tier never sets a spawner, so `foyer` uses
  `Handle::current()`: the test's runtime, or `main`'s. The thread that hangs is the one
  dropping that runtime -- a test whose assertion panics before `close`, or `pstore-server`'s
  `main` returning early (no replication source, cannot bind) or unwinding from a panic.
  The test hangs instead of failing; the server hangs instead of exiting.
- **`close` is not affected.** `foyer`'s `close` refuses new writes, then waits for its
  flushers and for every reclaim, so a runtime shut down after it drops no reclaim.
- **`foyer`'s flushers never exit.** Each holds its spawner, so a dedicated runtime handed
  over whole (`Spawner::Runtime`) is never released: about ten threads leak per core opened,
  closed or not. Measured on the way here.

## Delta

1. `Tiers::open` builds one multi-threaded runtime, 2 workers named `pstore-cache`, and hands
   its handle to all three classes' `foyer` builders (`with_spawner`).
2. `Tiers` owns the runtime as its last field, and **shuts it down in the background** when
   dropped: after the classes, never waited for, never dropped whole (dropping a runtime in
   another's async context panics).

So the deadlock, where `foyer` still reaches it, is on a worker nothing waits for, and the
caller's thread returns.

**Not changed:** `foyer` itself (the defect is upstream, reported), `close`, `wait`, what is
written or read, any block size or quota, and `pstore-server`'s shutdown order.

## Acceptance criteria

1. **`foyer`'s tasks run on the tier's workers.** `disk::tests::foyer_runs_on_the_tiers_workers`:
   a task spawned on each class's spawner runs on a thread named `pstore-cache`. Red first,
   without `with_spawner`.
2. **A dropped tier stops its workers.** `disk::tests::a_dropped_tier_stops_its_workers`: after
   the tier is dropped, a task spawned on its spawner never runs. Red first, with the runtime
   left to drop rather than shut down.
3. **The caller returns.** `disk::tests::a_runtime_dropped_mid_flood_returns`: 20 multi-threaded
   runtimes, each dropped with the tier open just after every class was flooded with eight
   times its share, at delays 0 to 380 µs; every one returns within 20 s. Red first without
   `with_spawner`: a quarter of such drops hang, so 20 all pass about 1 time in 300.
4. **No thread is left per core.** A core opened, closed and dropped 20 times leaves the
   process's thread count where it started (Linux, by hand; `NOT-RUN` elsewhere).
5. `./scripts/gates.sh` passes.

## Test plan

| # | Test | Fails first because | Mutation it catches |
|---|---|---|---|
| 1 | `foyer_runs_on_the_tiers_workers` | the spawner is the test's runtime: its worker is not named `pstore-cache` | `with_spawner` deleted |
| 2 | `a_dropped_tier_stops_its_workers` | dropping a runtime in async context panics; leaked, the task runs | `Workers::drop` emptied, or `mem::forget` |
| 3 | `a_runtime_dropped_mid_flood_returns` | the caller deadlocks in `foyer`'s `on_reclaim_finish` | `with_spawner` deleted |
| 4 | by hand | 208 threads after 20 cores with `Spawner::Runtime` | a spawner that owns the runtime |

⚠️ **Criterion 3 is a race, so it is red by probability, not by construction.** A first try
drove the burst through store reads: 0 of 400 drops hung, before the change as after, and it
was dropped. Inserting into the tier directly hangs 25 of 100. Criteria 1 and 2 pin the two
facts criterion 3 depends on, deterministically.

## RA budget

Unchanged: no blob request is added, removed or reordered. This is the local disk tier.

## Risks

- **Two more threads per core,** and a blocking pool. One core per process, so two threads.
- **A cross-runtime hop on a disk read.** `foyer`'s file reads already left the caller's
  worker for a blocking thread; they still do, on another runtime. Not measured.
- **The deadlock is not fixed, only moved.** Without `close`, a worker can still hang, holding
  `foyer`'s lock, for the life of the process. Nothing else takes that lock: the tier is gone.
- **`foyer` panics on the same path.** Shut down mid-flush, a flusher panics
  (`flusher.rs:384`, `unwrap` on its rotated buffer). tokio catches a task's panic; it is
  printed and does not reach the caller. Seen in the stress runs, not fixed here.
- **A wrong spawner could pass criterion 3 by luck,** 1 time in 300. Criterion 1 cannot.

## Tasks

- **M118.1** — the runtime, the tests, this ledger. One commit.
