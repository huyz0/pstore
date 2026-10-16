# M93 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container with 4 cores. Every number below is
`provisional` and relative.

Commands:
- `cargo test -p pstore-query --test parallel_masks`: 4 tests.
- `cargo test -p pstore-query -p pstore-engine -p pstore-server -p pstore-blob -p
  pstore-index`: 1163 tests, 0 failed.

1. **A mask does not run on the query's thread.** `masks_leave_the_querys_thread`: a filtered
   query over 16 segments on a 4-worker runtime, with a predicate no zone map prunes and no row
   passes, so every segment's mask reads its blocks and no row round follows. Not one mask read
   ran on the thread driving the query, and the answer under a predicate that admits every row
   equals the plain store's.
   - **Seen red on the code before M93**: "16 mask read(s) ran inline in the query's task".
   - ⚠️ The spec said "at least 2 distinct worker threads ran them". That assertion was dropped
     in review: leg reads use the same store method and already ran on workers, so it
     constrained nothing about masks. `inline == 0` is the check.
2. **An open does not run on the query's thread.** `opens_leave_the_querys_thread`: the 16
   footer reads (suffix reads) ran off the driving thread.
   - **Seen red on the code before M93**: "16 footer read(s) ran inline in the query's task".
   - ⚠️ The spec's "on at least 2 workers" was dropped for the same reason, and because sixteen
     microsecond reads can all land on one worker, which would flake on correct code.
3. **A dropped query aborts its masks and opens.** `a_dropped_query_aborts_its_masks` and
   `a_dropped_query_aborts_its_opens`: reads that never complete, the query dropped after
   100 ms, and within a further 100 ms none is in flight.
   - ⚠️ **Not seen red on the old code, by construction**: an inline future is cancelled when
     its query is dropped, so both pass before M93. They guard the spawned tasks.
   - **Killed by hand:** the abort in `Tasks`'s `Drop` replaced by a no-op made both fail; the
     file was then restored. This is the mutation the spec's test plan names.
4. **Answers unchanged.** `cargo test -p pstore-query -p pstore-engine -p pstore-server -p
   pstore-blob -p pstore-index`: every existing test passes, the depth and request-count tests
   among them. None was modified.
   - ⚠️ Depth is proven by `DepthCounting` under the single-threaded test runtime, as in M59.
5. **Measured, provisionally, and no gain is claimed.** `./scripts/split-bench.sh` exited 0 on
   2026-10-16 in a container otherwise idle (load average 2.3 before, 1.0 after):

   ```text
   layout: 48 segments x 2000 rows, dim 64, held [17, 16, 15], built in 6.8 s
    cpu  dense unsplit: p50    18.25 ms  p95    21.18 ms  parts/query 0.0
    cpu  dense   split: p50    18.42 ms  p95    23.18 ms  parts/query 2.0
    cpu hybrid unsplit: p50    20.68 ms  p95    23.79 ms  parts/query 0.0
    wait  dense unsplit: p50   104.67 ms  p95   114.03 ms  parts/query 0.0
    wait hybrid unsplit: p50   107.92 ms  p95   113.33 ms  parts/query 0.0
   ```

   - The split-to-unsplit ratios were 1.00–1.16 across dense, text and hybrid, as at M59.
   - ⚠️ **The benchmark cannot show this change**: it runs no filter, so no mask, and its 48
     small segments make the open round's decode a few milliseconds at most. No before/after pair
     was taken on M93's own code, so nothing here says the change sped anything up. An earlier
     run overlapped another job (load average 8) and was discarded.
6. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - **Mutation:** `./scripts/mutants.sh` over the committed diff (`MUTANTS_BASE=HEAD~1`),
     against `pstore-query`'s changed file and `pstore-engine`'s one changed line: **18 mutants,
     4 caught, 14 unviable, 0 missed, 0 timed out**. The caught: `part`, `candidates` and `run`
     each replaced by an empty answer, and the generic tasks' join by an empty list.
   - ⚠️ The sweep's mutants are whole-function replacements only. The finer ones this change
     could have (the masked-segment condition, the order of results) were unviable or not
     generated, so they rest on the answer tests in 4 and on code review.
   - **Code review:** one independent round, no blocking or major findings. Its two minors, both
     on the `workers >= 2` assertions, were acted on as 1 and 2 say.

**Not covered**, as the spec states:
- the row round's own decoding (`fetch_rows`, `resolve_rows`);
- the tasks compete with other queries for the runtime's workers;
- depth, as criterion 4 says;
- no measurement shows a latency gain (criterion 5).
