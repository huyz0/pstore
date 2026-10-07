# M59 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container with 4 cores. Every number below is
`provisional` and relative.

Commands:
- `cargo test -p pstore-query --test parallel_legs`: 3 tests.
- `cargo test -p pstore-blob -p pstore-query -p pstore-engine -p pstore-server -p
  pstore-index`: every test passes.

1. **Segments are scored in parallel.** `legs_are_scored_in_parallel`: 16 segments scanned
   exactly, on a 4-worker runtime, with a store whose range reads hold their thread for 25 ms.
   - The answer equals the unblocked query's.
   - At least 2 leg reads were in flight at once, on at least 2 threads.
   - The query took under half the serial time.
   - **Seen red on the code before M59**, with `run.rs` reverted to the committed version:
     "at most 1 leg read(s) ran at once". Before the overlap check was
     added (spec review), the timing alone was seen red: 16 reads of 25 ms took 410 ms.
2. **A dropped query aborts its legs.** `a_dropped_query_aborts_its_legs`: with leg reads
   that never complete, the query's future is dropped after 100 ms, and within a further
   100 ms no read is in flight. Killed by hand: the abort in `Tasks`'s `Drop` removed.
   - Added in code review, `a_legs_panic_is_the_querys`: a leg's panic on its own task is
     resumed in the query's. Killed by hand: the panic arm made unreachable, which turned it
     into an error.
3. **Answers unchanged.** `cargo test -p pstore-blob -p pstore-query -p pstore-engine -p pstore-server -p pstore-index`:
   every existing test passes, the depth and request-count tests among them. None was
   modified.
   - ⚠️ Depth is proven by `DepthCounting` under the single-threaded test runtime, as every
     depth test runs (spec review). On a multi-threaded one, that meter could read a leg that
     finished before a sibling's read began as a deeper round, though no request waits on
     another.
4. **Measured, provisionally.** `./scripts/split-bench.sh` exited 0 on 2026-10-07, with no
   build or sweep running. Load average was 1.43 before and 1.07 after:

   ```text
   layout: 48 segments x 2000 rows, dim 64, held [18, 16, 14], built in 4.4 s
    cpu  dense unsplit: p50    13.74 ms  p95    17.08 ms  parts/query 0.0
    cpu  dense   split: p50    14.51 ms  p95    17.69 ms  parts/query 2.0
    cpu  dense   ratio: split/unsplit p50 = 1.06
    cpu   text unsplit: p50     3.78 ms  p95     6.20 ms  parts/query 0.0
    cpu   text   split: p50     5.13 ms  p95     8.18 ms  parts/query 2.0
    cpu   text   ratio: split/unsplit p50 = 1.36
    cpu hybrid unsplit: p50    16.95 ms  p95    20.70 ms  parts/query 0.0
    cpu hybrid   split: p50    16.71 ms  p95    19.73 ms  parts/query 2.0
    cpu hybrid   ratio: split/unsplit p50 = 0.99
   wait  dense unsplit: p50   100.98 ms  p95   104.46 ms  parts/query 0.0
   wait  dense   split: p50   100.35 ms  p95   103.17 ms  parts/query 2.0
   wait  dense   ratio: split/unsplit p50 = 0.99
   wait   text unsplit: p50    90.30 ms  p95    91.97 ms  parts/query 0.0
   wait   text   split: p50    91.85 ms  p95    95.35 ms  parts/query 2.0
   wait   text   ratio: split/unsplit p50 = 1.02
   wait hybrid unsplit: p50   102.31 ms  p95   105.20 ms  parts/query 0.0
   wait hybrid   split: p50   103.52 ms  p95   107.37 ms  parts/query 2.0
   wait hybrid   ratio: split/unsplit p50 = 1.01
   ```

   **Against M57's run of the same benchmark** (one host, provisional):
   - An unsplit dense query's CPU went from 49.2 to 13.7 ms, and hybrid from 55.5 to
     17.0 ms. Its 20 ms-wait time went from 138 to 101 ms: four waits and about 21 ms of the
     rest.
   - **On one host, splitting now buys nothing** (ratios 0.99–1.36). A single server's
     scoring already uses every core, and the split only adds its exchanges. What M57
     measured as the split's gain here was mostly the serial scoring spread over three
     processes. On separate machines a split still adds cores and bandwidth. That is backlog
     row 59, open for M0b.
   - M57's ledger carries a correction banner pointing here.
5. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - `cargo deny check`: licences, bans and sources pass. `tokio` was already in the lock file
     and is now a direct dependency of `pstore-query`. ⚠️ Advisories fail on `paste`
     (RUSTSEC-2024-0436) through `foyer`, as at M53–M58: not this change.
   - **Mutation:** `pstore-query/src/run.rs`, `pstore-engine/src/lib.rs` and
     `pstore-blob/src/store.rs`, against the libraries' tests and `parallel_legs`,
     `multi_segment`, `hybrid`, `fusion`, `split` and `engine_query` (`--no-config --profile
     mutants`, `-j 1`), over the tree with code review's fixes: **26 mutants, 2 missed**, 14
     caught, 10 unviable.
   - **The 2 misses:**
     - `deleted + shadow` as `*` in the widening, a line this change only re-indented: killed
       by `a_mostly_superseded_clustered_segment_still_answers_top_k` and
       `text_and_sparse_legs_are_widened_past_deleted_rows_too`, in `pstore-engine`'s upsert
       tests, which the sweep did not run. Seen red by hand.
     - The panic guard `e.is_panic()` as `true`: ⚠️ **equivalent, not tested**. It differs only
       on a cancelled task. Only `Tasks`'s own `Drop` cancels one, and it cannot run while
       `join` waits. A runtime shutting down cancels the query's task too, so nothing is left
       to see the difference.

**Not covered**, as the spec states:
- masks, and the open round's footer decoding, stay serial (backlog row 62);
- scoring competes with other queries for the runtime's workers;
- depth, as criterion 3 says.
- From code review, accepted:
  - the cancelled arm of `Tasks::join` is reached only by a runtime shutting down under a
    query, and no test does that;
  - `scan_part` wraps its statistics in an `Arc` by cloning them, which on the coordinator's
    failure path copies the whole vocabulary once per failed share.
