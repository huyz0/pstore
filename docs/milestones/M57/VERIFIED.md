# M57 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Every number here is `provisional`.** The host was a Linux x86-64 cloud container with 4
cores. All three servers and the unpeered one ran in one process over one in-memory store, on
loopback, and the `wait` backend's latency is an injected 20 ms sleep. The numbers are relative,
one host's cores are shared by every server, and none of them is a latency of a real fleet or
store. Backlog row 59 stays open for that, on M0b.

1. **The benchmark runs.** `./scripts/split-bench.sh` exited 0 on 2026-10-06, with no build or
   sweep running. Load average was 1.41 before and 1.02 after. The full output:

   ```text
   layout: 48 segments x 2000 rows, dim 64, held [8, 20, 20], built in 7.7 s
    cpu  dense unsplit: p50    49.22 ms  p95    64.40 ms  parts/query 0.0
    cpu  dense   split: p50    36.40 ms  p95    45.33 ms  parts/query 2.0
    cpu  dense   ratio: split/unsplit p50 = 0.74
    cpu   text unsplit: p50     9.20 ms  p95    12.52 ms  parts/query 0.0
    cpu   text   split: p50    10.76 ms  p95    16.98 ms  parts/query 2.0
    cpu   text   ratio: split/unsplit p50 = 1.17
    cpu hybrid unsplit: p50    55.47 ms  p95    84.92 ms  parts/query 0.0
    cpu hybrid   split: p50    41.00 ms  p95    50.65 ms  parts/query 2.0
    cpu hybrid   ratio: split/unsplit p50 = 0.74
   wait  dense unsplit: p50   138.17 ms  p95   158.10 ms  parts/query 0.0
   wait  dense   split: p50   114.10 ms  p95   134.31 ms  parts/query 2.0
   wait  dense   ratio: split/unsplit p50 = 0.83
   wait   text unsplit: p50    98.01 ms  p95   106.29 ms  parts/query 0.0
   wait   text   split: p50    95.80 ms  p95   100.63 ms  parts/query 2.0
   wait   text   ratio: split/unsplit p50 = 0.98
   wait hybrid unsplit: p50   144.68 ms  p95   171.20 ms  parts/query 0.0
   wait hybrid   split: p50   116.65 ms  p95   124.90 ms  parts/query 2.0
   wait hybrid   ratio: split/unsplit p50 = 0.81
   ```

   **What it shows, provisionally:**
   - Split three ways on one host, a dense or hybrid query's p50 was 0.74–0.83 of the
     unsplit one's, in both backends.
   - A text query, the cheapest here at 9–10 ms, gained little or nothing. Its two exchanges
     cost about what its third of the work saves.
   - ⚠️ **Noise is of the order of the text gain.** An earlier run of the same benchmark
     (before this one, with the same code) gave a text `cpu` ratio of 0.87 where this one
     gives 1.17, and dense 0.79 where this one gives 0.74. Read one decimal, never two.
   - ⚠️ **The layout is not fixed.** The corpus and queries come from fixed seeds, but
     `assign` hashes the servers' URLs, which carry ports the OS picks. So the segments each
     server held differed between runs: 16/17/15 earlier, 8/20/20 here.
   - ⚠️ **Corrected by [M59](../M59/VERIFIED.md):** the "seven waits" below were four blob
     rounds plus about 48 ms of scoring run serially in one task, about 1 ms a segment. There
     was no queueing and no extra request. M59 scores segments in parallel. Much of the split's
     gain measured here was that serial scoring spread over three processes.
   - Under `wait`, an unsplit dense query took about 138 ms, roughly seven waits of 20 ms
     where D-34 counts four rounds. This benchmark does not say why: the extra wait may be
     requests queued behind a concurrency limit. A split query's 114 ms is consistent with
     each server queueing less. This is an observation for M0b to explain, not a finding.
2. **A wrong split answer fails the run.** `split_bench` in
   `crates/pstore-server/tests/split_bench.rs` compares every split answer's results with the
   unpeered server's.
   - Seen red by hand: in `pstore-query/src/run.rs`, both `Ok(hits) =>
     candidates_found.extend(hits)` arms (the phased shares' and the one-exchange shares')
     were edited to drop the hits. The run then failed on its first split query: "cpu dense:
     the split answer differs".
   - No mutant reproduces this edit, which is why it is recorded here.
3. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit. It compiles and lints
     the benchmark, and never runs it (`#[ignore]`).
   - `scripts/check-portable.sh` accepts `scripts/split-bench.sh`.
   - Every number above is marked provisional.

**Not covered**, as the spec states: separate machines, a real object store, and a read
cache. Backlog row 59 stays open for them, on M0b.
