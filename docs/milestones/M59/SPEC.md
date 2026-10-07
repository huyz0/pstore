# M59 — Every segment's leg on a task of its own

**Serves:** the project owner's request of 2026-10-07 to spec and deliver the fix for what
[M57](../M57/VERIFIED.md)'s benchmark left unexplained. It closes backlog row 61, which this
milestone adds. Depth stays D-34's.

## What is true today

Measured on 2026-10-07 in this container, one unsplit dense query over 48 segments with every
store request waiting 20 ms. The probe was a request timeline, a throwaway test not in the tree:

- **The blob depth is D-34's four rounds, exactly.**
  - Round 0: HEAD.
  - Round 1, at 20 ms: 48 footers and 48 centroid tables.
  - Round 2, at 40 ms: 48 leg reads.
  - Round 3, at about 109 ms: 10 row fetches.

  No other request is made.
- **Round 2's reads, issued at 40 ms, finished at about 108 ms instead of 60 ms.** The 48 ms
  between is scoring. `candidates` (`pstore-query/src/run.rs`) joins every `(segment, leg)`
  future with `try_join_all`, which polls them all inside one task. So each leg's scoring,
  which runs in the index's search right after its read, holds the only thread the query
  has. Meanwhile the other legs' finished reads wait to be polled.
- **It is linear in segments:** about 1 ms each at 2,000 rows and 64 dimensions. 48 segments
  took 48 ms, and a coordinator scanning 17 took 16 ms. M57's `cpu` backend measured 49 ms for
  the same query, which plus four waits is the 129–138 ms observed.
- **The server's runtime has four worker threads, and a query uses one.** The split of M54
  sped dense queries up partly by spreading this serial work over processes.

## Delta

**Each `(segment, leg)` search of a query runs as a task of its own, so the runtime's worker
threads score segments in parallel. Answers, requests, bytes and depth are unchanged.**

1. **Owned inputs.** A spawned task must own what it reads:
   - **A store handle.** The query functions that run legs take `S: BlobStore + Clone`. A
     blanket `impl BlobStore for Arc<S>` (replacing today's `Arc<dyn BlobStore>` impl, with
     every method forwarded) lets the engine pass its `Arc<S>`, and the engine's `Split` store
     is `Clone`.
   - **The segment's footer and sidecars:** a clone of the opened segment without its delete
     vector, which a leg does not read.
   - **The leg**, rebuilt as an owned `Prefetch`, widened or made exhaustive inside the task
     exactly as today.
   - **The statistics**, moved into one `Arc` the query's tasks share. They are never copied
     per task, since the coordinator's own hold its whole vocabulary, and never cut (spec
     review): a term missing from a cut would fall back to the segment's own `df`, D-30's bug,
     silently. `scan_part` wraps the statistics it is given, which M55 already cut.
   - The segment's key, its ordinal and the full-text schema (`FullText` is `Copy`).
2. **Order.** Results are collected in the order the futures were made, as `try_join_all`
   returns them today, so every later step sees the same sequence.
3. **Errors and panics.**
   - A leg's error is the query's error, as today.
   - On the first error the remaining tasks are aborted, by the same guard as rule 4, as
     `try_join_all` dropped the remaining futures.
   - A panic in a leg is resumed in the query's task, as an inline panic would have been.
   - A task cancelled under the query, by a runtime shutting down, is an error, never a
     panic.
4. **A dropped query aborts its legs.** The tasks are aborted when the query's future is
   dropped, by a client hanging up or a peer's timeout. Spawned tasks otherwise outlive it,
   reading and scoring for nobody.
5. **Masks stay inline.** A filter's mask is still computed in the query's task. That is a
   separate CPU step, smaller than scoring, and left to the backlog.

**API changes** (spec review):
- `pstore-query` gains `tokio` (`rt`) as a dependency. Its query functions now spawn, so they
  must run inside a tokio runtime. Every caller in the tree already does.
- `query`, `query_with`, `query_rows_filtered`, `query_rows_split`, `part` and `scan_part`
  require `S: BlobStore + Clone`.
- `impl BlobStore for Arc<S>` replaces the impl for `Arc<dyn BlobStore>`, which it covers.
  `pstore-blob/tests/dyn_store.rs` pins that every method forwards.

**Not changed:** the format, the HTTP API, the answers, the request count, the bytes, the
depth, and the split's protocol.

**Not covered, and the ledger says so:**
- masks, and the open round's footer decoding, stay serial;
- the scoring now competes with other queries for the same worker threads: parallel within a
  query, never more cores than the runtime has;
- depth is proven by `DepthCounting` under the single-threaded test runtime, as every depth
  test runs. On a multi-threaded one, a leg finishing before a sibling's read begins would
  read as a deeper round to that meter, though no request waits on another.

## Acceptance criteria

1. **Segments are scored in parallel.** `legs_are_scored_in_parallel`, in
   `crates/pstore-query/tests/parallel_legs.rs`:
   - 16 segments scanned exactly. There is no centroid table, so the dense leg reads its
     vectors by range.
   - A store whose range reads block their thread for 25 ms, standing in for scoring that
     holds a thread. The open round reads footers by suffix and is not blocked.
   - A multi-threaded runtime of 4 workers.

   The answer equals the unblocked one, and:
   - **deterministically, at least 2 leg reads were in flight at once**, and they ran on at
     least 2 threads;
   - secondarily, the query took under half the serial time, which is 25 ms times the leg
     reads counted.

   It is seen red on today's code.
2. **A dropped query aborts its legs.** `a_dropped_query_aborts_its_legs`:
   - leg reads that never complete, each counted while in flight;
   - the query's future dropped after 100 ms;
   - within a further 100 ms, none is in flight.

   It is seen red with the abort-on-drop guard removed.
3. **Answers unchanged.** Every existing test of `pstore-query`, `pstore-engine` and
   `pstore-server` passes unmodified, the depth and request-count tests among them.
4. **Measured, provisionally.**
   - `scripts/split-bench.sh` is rerun and its output recorded in the ledger, with the same
     container caveats as M57.
   - M57's ledger gains a correction banner: the "seven waits" are explained here.
5. **Gates:**
   - `./scripts/gates.sh` passes;
   - the incremental mutation sweep of the changed lines misses 0, or each miss is killed by a
     test named in the ledger.

## Test plan

| AC | Seen red first under |
|---|---|
| 1 | today's `try_join_all` of inline futures |
| 2 | the abort guard removed |

## RA budget

Unchanged: W, Rseq, Rpar, List, bytes and depth. Only where the CPU runs changes.

## Risks

- **A leg's inputs cloned per task.** A segment footer is cloned once per leg, and
  `VecIndex::from_parts` already clones it for every dense leg. Sidecars are `Bytes`, cheap to
  clone. The statistics are one cut per query. AC1's timing and the benchmark would reveal a
  regression.
- **`S: Clone` reaching callers.** Every store a query function is given must be `Clone`. The
  compiler reveals each one. Those in the tree already are, except the engine's `Split`,
  which gains a hand-written `Clone` of its two `Arc`s.

## Tasks

- **M59.1** Owned leg inputs, the spawn and abort guard, `Arc<S>` as a store, the tests, the
  benchmark rerun, M57's correction, and backlog row 61: AC1–AC5.
