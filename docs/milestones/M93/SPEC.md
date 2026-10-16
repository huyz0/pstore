# M93 — Masks and the open round on tasks of their own

**Serves:** backlog row 62, opened by [M59](../M59/VERIFIED.md), which put each `(segment, leg)`
search on a task of its own and left these two steps serial. Depth stays D-34's.

## What is true today

A query's scoring runs on the runtime's worker threads (M59). Two linear-in-segments CPU steps
still run in the query's own task, polled one after another by `try_join_all`:

- **A filter's mask** (`mask` in `pstore-query/src/run.rs`): the segment's blocks are fetched
  concurrently, but each block's decoding and the predicate's `admits` over every row run when
  that future is polled, on the one thread the query holds.
- **The open round** (`open`): the footer, the delete vector and the sidecars are fetched
  concurrently, and `Segment::open_at` decodes each footer in the query's task.

Neither is measured here; the criteria below do not depend on a number, only on where the work
runs. The M59 benchmark is rerun to see whether the change shows (criterion 4).

## Delta

**Each segment's mask, and each segment's open, runs as a task of its own. Answers, requests,
bytes and depth are unchanged.**

1. **A mask is a task.** `candidates` spawns one task per masked segment, owning a store handle,
   the segment's key, a clone of the opened segment, and the predicate (`Predicate: Clone`; it is
   cloned once per masked segment, which is a tree of comparisons).
2. **An open is a task.** `open` is spawned per target in `part`, `open_part` and `run`, owning a
   store handle, the target and the legs' prefetch (an `Arc<[Prefetch]>`, never cloned a task).
   `open_part` gains `S: Clone`, as `part` has had since M59.
3. **One guard.** `Tasks` (M59's abort-on-drop, join-in-order, panic-resume) becomes generic in
   the task's result, so the mask and open tasks inherit all three rules unchanged: a dropped
   query aborts them, the first error is the query's and aborts the rest, a panic is resumed in
   the query's task, and a cancelled task is an error.
4. **Order.** Results come back in the order the tasks were made, as `try_join_all` returned them.

**Not changed:** the format, the HTTP API, the answers, the request count, the bytes, the depth,
the split's protocol, and the row round's own decoding (`fetch_rows`, `resolve_rows`), which is
smaller than either step above and left to the backlog if a number asks for it.

**Not covered, and the ledger says so:** the tasks compete with other queries for the same
worker threads: parallel within a query, never more cores than the runtime has.

## Acceptance criteria

1. **A mask does not run on the query's thread.** `masks_leave_the_querys_thread`, in
   `crates/pstore-query/tests/parallel_masks.rs`: a filtered query over 16 segments on a
   4-worker runtime, against a store whose reads record the thread they ran on. Every mask block
   read, which is a range read of a segment's data blocks, ran on a thread other than the one
   driving the query. Answer equals the
   unfiltered store's. Seen red on today's code.
2. **An open does not run on the query's thread.** `opens_leave_the_querys_thread`, same file: the
   footer reads (suffix reads) of 16 segments ran off the driving thread. (No count of worker
   threads is asserted: sixteen microsecond reads can all land on one worker, and the test would
   flake on correct code.)
   Seen red on today's code.
3. **A dropped query aborts its masks and opens.** `a_dropped_query_aborts_its_masks` and
   `a_dropped_query_aborts_its_opens`: reads that never complete, the query dropped after 100 ms,
   and within a further 100 ms none is in flight. Seen red with the abort-on-drop removed.
4. **Answers unchanged.** Every existing test of `pstore-query`, `pstore-engine` and
   `pstore-server` passes unmodified, the depth and request-count tests among them.
5. **Measured, provisionally.** `scripts/split-bench.sh` rerun, output in the ledger with the
   container caveats of M57. The number is reported either way; no bound is claimed.
6. **Gates:** `./scripts/gates.sh` passes; the incremental mutation sweep of the changed lines
   misses 0, or each miss is killed by a test named in the ledger.

## Test plan

| AC | Seen red first under | Mutation it catches |
|---|---|---|
| 1 | today's inline `mask` futures | mask spawned but awaited inline in the query task |
| 2 | today's inline `open` futures | open spawned for `run` but not `part` |
| 3 | `Tasks`'s `Drop` abort removed | a guard that does not abort |

## RA budget

Unchanged: W, Rseq, Rpar, List, bytes and depth. Only where the CPU runs changes.

## Risks

- **A clone a task.** The opened segment's footer is cloned per masked segment, and the predicate
  once per masked segment. Criterion 5 would reveal a regression.
- **`S: Clone` reaching `open_part`.** The compiler reveals each caller; the engine's store is
  already `Clone`.
- **Spawning costs more than the work it moves** on a segment too small to matter. A task is
  microseconds; a footer decode is not smaller on any segment this system writes.

## Tasks

- **M93.1** Generic `Tasks`, spawned masks and opens, the tests, the benchmark, the ledger.
