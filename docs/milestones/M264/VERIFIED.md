# M264 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.
Changes from the spec while building, all stated in the spec:
- AC2's "misses equal to the bytes fetched" became "no more than `cost.bytes_read`": the
  read count also holds uncached whole-object reads such as the HEAD, so the two cannot be
  equated through the API.
- A single-flight waiter's test was dropped. On a current-thread runtime, two joined reads
  of one range don't overlap against the in-memory store, so no test made them overlap
  deterministically.
- AC5 is a measured bound, not an A/B of two builds: no switch can compile the counters out
  of a library the example links.

How to run the tests: `cargo test -p pstore-cache --test it traffic` and
`cargo test -p pstore-server --test it performance::`.

Red runs. Against the parent, AC1 to AC4 cannot compile: there is no `Caching::traffic` and
no `meta.performance`. Each test was also seen red by the mutations below. A script applied
each mutation, ran both tests and reverted it. No `cargo mutants` run is claimed. All seven
were killed.

1. `performance_is_reported`: green for a query and a multi-query. Killed:
   `query_execution_ms` reported as 0 (P1); the two times swapped (P2).
2. `cache_ratio_cold_then_warm`: green. A second server's cold query misses, no more than
   its `cost.bytes_read`, at a ratio below 1. Warm, it misses 0 and hits the cold total, at a
   ratio of 1.0, with the same results. Killed: the ratio taken over misses alone (P4).
3. `no_cache_reports_null_ratio`: green, cold and warm. Killed: an uncached view counting
   its reads (P5), which `traffic_counts_each_cacheable_read`'s uncached view also kills.
4. `traffic_counts_each_cacheable_read`: green for `get_range`, `get_ranges` (a repeated
   range and two ranges within the coalescing gap), `get_suffix` and `get_immutable`, cold
   then warm, with `get` not counted. Killed: hits taken as asked bytes (P3); a batch's miss
   counted once for a range asked twice (P6); `get` counted (P7).
5. Overhead, `cargo run --release -p pstore-server --example perf_overhead`, `provisional`
   (one container, in-memory store). Over 20,000 rows and 2,000 warm cached queries through
   the router:
   - The median warm query took 3,301.9 µs.
   - Per operation: a clock read 22.7 ns, an `elapsed` 27.4 ns, a relaxed load 0.3 ns, a
     relaxed add 7.5 ns, and the five fields' serialisation 155.4 ns.
   - The cold query made 5 store reads, which bounds its cache calls.
   - M264 adds 294.3 ns per query, **0.009 %** of the median, under the 1% bound.

   If single-flight let the calls exceed the store reads twentyfold, the share would still be
   under 0.02%. So by the settings rule it ships without a setting.
6. Gates: `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features --
   -D warnings` pass. `scripts/gen-clients.py --check` passes after regenerating the clients
   for `Performance`, and the `openapi` contract tests pass. The workspace suite at this
   commit, `cargo test --workspace --no-fail-fast -j 3`, ran 56 test binaries: 2,893 passed,
   0 failed, 15 ignored. The two IPv6 tests were NOT-RUN, as in M263, because this container
   has no IPv6 loopback. `cargo deny check`, coverage and the mutation sweep were NOT-RUN
   here; CI runs them on the PR.

## After CI's mutation shard (PR #65)
Shard 11 found two mutants no test killed: `Performance::of`'s `* 1e3` replaced by `+` for
`server_total_ms` and by `/` for `query_execution_ms`. The server tests only read the
fields' presence and order. `performance_spans_are_milliseconds` in `types.rs` builds one
from a start 50 ms back and a 20 ms execution: `query_execution_ms` is 20, `server_total_ms`
lies in [50, 900), the ratio is 0.75 of 3 hit and 1 missed bytes. Both mutants, applied by
hand, turn it red; restored, it passes.
