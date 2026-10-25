# M123 verification

Runs use Linux Docker on WSL2, 8 GiB, Rust 1.90. Final gates used six CPUs;
benchmarks used four. Timings are provisional.
The reviewed base is 4654acb; obsolete work on 0337112 remains in a local stash.

1. `concurrency_admission_blocks_and_releases_on_cancellation`: red on current main
   (second backend operation entered at ceiling one), then green via
   `cargo test -p pstore-blob --lib`.
2. `a_reduced_limit_blocks_new_admissions`, `zero_concurrency_still_allows_progress`,
   `recovery_at_the_largest_ceiling_does_not_overflow`, and
   `the_success_counter_can_cross_its_integer_boundary`: all red on current main,
   then green with the same blob command. The old integer edges panicked or left
   a zero limit; the normal recovery and reduction bounds remain intact.
3. `releasing_capacity_wakes_a_conditional_write_waiter`: red on current main
   (conditional write bypassed admission), then green with the same command.
   The test observes an actual Wake signal after releasing occupied capacity.
4. `cargo test --workspace --all-features --quiet` passed within the completed
   fast-gate run, `bash scripts/gates.sh` (all gates green). The
   benchmark after M123/M124, before M125, matched all seven baseline digests
   via `bash scripts/query-bench.sh`. Dense/text/hybrid p50 were 0.989/0.542/1.049 ms
   against 0.954/0.571/1.091 ms before. These single-run differences are noise,
   not a speedup claim; this bench's backend is MemoryStore, without Congested.

## Limiter microbenchmark

`cargo test --release -p pstore-blob --lib admission_microbench -- --ignored --nocapture`,
five samples of 100,000 heads: baseline limited median 53.3 ns, fixed median 108.7 ns.
Raw heads were about 30 ns. Enforced admission adds about 55.4 ns per uncontended
operation here; no cloud latency or throughput claim follows. Retries and request
counts are unchanged. The benchmark is ignored by ordinary tests.

Independent code review: one completed round, no blocking or major findings.
Coverage and full mutation sweeps were NOT-RUN in this review; no floor changed.
