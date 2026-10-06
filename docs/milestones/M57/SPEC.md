# M57 — What a split buys, measured provisionally

**Serves:** [BACKLOG](../BACKLOG.md) row 59, which the project owner asked on 2026-10-06 to
close with the rest of the backlog.

## What is true today

Read from the tree at M56, nothing measured:

- **The evidence for M54–M55 is structural.** The tests show who read what, that every answer
  equals the single-server answer, and that depth stays four blob rounds. No test shows a split
  query answering faster.
- **Nothing can measure the real gain here.** A split's gain is each server's CPU and network
  working on a third of the segments. That needs several machines over a real object store,
  and `slos.md` blocks every latency on M0b (cloud).
- **This container can measure two things:**
  - **The overhead.** With one store and an injected wait per request, the split adds a
    server-to-server exchange (two for a text leg) beside the open round. Its cost against an
    unsplit query is measurable.
  - **CPU parallelism on one host.** Three in-process servers on 4 cores can each scan their
    third concurrently, which a single coordinator does on its own tasks. This bounds nothing
    about separate machines, but it shows whether the split serialises anything.

## Delta

**A benchmark, run by `scripts/split-bench.sh`, times the same queries against one unpeered
server and against a coordinator of three peered servers. All of them run in one process,
over one in-memory store, on loopback. The answers must be equal. Its numbers go in the
ledger marked `provisional`, never as an objective.**

1. **Corpus:** 48 segments of 2,000 rows each, 96,000 rows in all. Each row has a 64-dimension
   vector and a text of a few words, from a fixed seed, and the queries come from another.
   Every segment is clustered.
   - Before measuring, the benchmark asserts the segment count and that `assign` gives every
     server some segments. It prints both.
2. **Queries:** dense, text, and hybrid by RRF. Each kind runs 50 times after 5 warm-up runs,
   on each of two backends:
   - **`cpu`:** no injected wait, so the time is CPU and copying;
   - **`wait`:** 20 ms injected per store request, so the time is rounds.
3. **Reported:** one line per (backend, query kind, layout), giving:
   - p50 and p95 in milliseconds over 100 runs (p95 is the sixth-worst);
   - the parts sent per query, from `pstore_peer_parts_sent`.

   Each unsplit/split pair also reports the ratio of their p50s.
   - The runs counted are 100, after 5 warm-up runs.
   - `wait` sets the delay on every server's store, the unpeered one's included.
4. **Checked while measuring:** every split answer equals the unsplit one. A mismatch fails
   the run, so a fast wrong answer never reaches the ledger.
5. **How it runs.**
   - `scripts/split-bench.sh` runs `cargo test --release -p pstore-server --test split_bench
     -- --ignored --nocapture`.
   - The test uses a multi-threaded runtime with 4 workers (spec review). Query CPU work never
     leaves its task, so on one thread the split could show no parallelism at all.
   - A test rather than an example, because the cluster helpers live beside the tests.
6. **Not a gate.** It runs only from the script (`#[ignore]`d), never from `gates.sh`, and
   asserts no latency bound: per AGENTS.md, a number from one container is relative and
   provisional.
   - The ledger records the host (cores, container) and the load average, measured with no
     build or sweep running.

**Not changed:** any code outside the benchmark.

**Not covered, and the ledger says so:** latency on separate machines, a real object store,
and a cache. Backlog row 59 stays open for the measurement on M0b's cloud fleet. This
milestone closes only the in-container part.

## Acceptance criteria

1. `scripts/split-bench.sh` exits 0 and prints the layout line and 12 measurement lines (2 backends × 3 query kinds
   × 2 layouts) in the stated format, plus a ratio for each of the 6 unsplit/split pairs.
   Test: running it. The command and its output go in the ledger.
2. The benchmark fails on a split answer unequal to the unsplit one. It is seen red with the
   coordinator dropping its peers' hits, a hand edit to `run` that the ledger records, since
   no mutant reproduces it. Test: `split_bench`, in
   `crates/pstore-server/tests/split_bench.rs`, run by the script.
3. **Gates:**
   - `./scripts/gates.sh` passes;
   - `scripts/check-portable.sh` accepts the new script;
   - the ledger labels every number `provisional`.

## Test plan

| AC | Seen red first under |
|---|---|
| 2 | the coordinator dropping its peers' hits |

## RA budget

Unchanged: no shipped code changes.

## Risks

- **A container's numbers read as a promise.** Mitigated by the label, the ratio-only
  framing, and row 59 staying open for M0b.
- **Noise.** Four cores are shared with the build. 50 runs per cell and p50/p95, never the
  mean.

## Tasks

- **M57.1** The benchmark, the script, and the ledger: AC1–AC3.
