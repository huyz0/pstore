# M276: per-feature microbenchmarks, reproducible

## Serves
The plan's [evaluation](../../comparisons/turbopuffer-next-plan.md) "Microbenchmarks" row and
DELIVERY's "Reproducible per-feature CPU/allocation/quality microbenchmarks" (parser campaign
in M126 only). The cloud architecture experiment and the TCO model stay blocked on cloud
accounts. Builds on M119's `query-bench.sh`; no decision is reopened.

## Delta
A new benchmark, `scripts/microbench.sh` (release, `cargo test --ignored` of
`crates/pstore-server/tests/microbench.rs`), one server in process on the in-memory store, so
it measures server CPU and allocation with no network and no store latency:
- **Writes**, 500 rows of 64 dimensions a batch, `durable`: rows with array f32 vectors, the
  same rows with base64 f32 vectors, the same as `upsert_columns`, and text-only rows.
- **Queries** over a fixed corpus (4 folded segments of 2,000 rows: a 64-dimension vector, a
  multi-vector field of 2 x 8, a sparse field, text, two attributes): BM25, filter with order,
  BM25 with a filter, sparse dot product, MaxSim, dense ANN, a two-leg multi-query fused by RRF,
  `compute_attributes` with `VectorDist`, and a highlight.
- Every case: fixed seeds; warmups, then repetitions; median, p10, p90 and the median absolute
  deviation of its time; allocations and allocated bytes per operation (`stats_alloc` as the
  binary's allocator); a digest of every answer (ids and scores, or a write's counts), so a
  faster run that answers differently is a changed digest. The header names the commit, the
  compiler, the CPU and its cores. Results are also written as CSV.

Not a gate, as the plan says, until its noise is known: no threshold, not in `gates.sh`, not in
CI. Its numbers are `provisional` (one host, in process).

Unchanged: every product path; M119's benchmark.

## Acceptance criteria
1. `./scripts/microbench.sh` runs all 13 cases and prints, per case, median, p10, p90, MAD,
   allocations and bytes per operation and a digest, and the header line; it writes the CSV.
2. Two consecutive runs give the same digest for every case.
3. The run-to-run spread of each median is recorded in VERIFIED.md, with the host.
4. The benchmark is skipped by `cargo test` (ignored) and by the mutation sweep.

## Test plan
1. Run it; the output and CSV are its evidence. An answer that is empty or a non-200 fails
   the run (asserted), so a case cannot measure nothing.
2. Run it twice; compare digests (VERIFIED).
3. The same two runs.
4. `cargo test -p pstore-server --test microbench` reports it ignored; `scripts/mutants.sh`
   excludes test files.

## RA budget
None: a benchmark, no product change.

## Risks
- In-process numbers miss the network and the store; that is the architecture experiment's
  job, which needs a cloud. Allocation counts include the router and the test's own request
  bodies' reads, the same for every case.

## Tasks
- M276.1 the spec, the benchmark and its script, a recorded run, VERIFIED.
