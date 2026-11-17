# M276 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.
No product code changes, so there is no red run of a product test: a benchmark's evidence is
its output. Every case asserts a 200 and a non-empty answer, so none can measure nothing.

Host (`provisional`): a 4-core cloud container, `Intel(R) Xeon(R) Processor`, rustc 1.90.0,
release profile, commit `1929bf7`, nothing else building. Corpus: 4 folded segments of 2,000
rows, 64 dimensions, a 2 x 8 multi-vector field, a sparse field, text and two attributes;
writes of 500 rows; 5 warmups, 40 runs a write and 100 a query.

1. `./scripts/microbench.sh`, run twice: all 13 cases printed median, p10, p90, MAD,
   allocations and bytes per operation and a digest, under the header line, and the CSV was
   written to the path MICROBENCH_CSV names. Its test is `microbench`.
2. `./scripts/microbench.sh`, both runs: every case's digest the same, and every case's allocations per operation the
   same to the allocation.
3. `./scripts/microbench.sh`, medians in ms, first run then second, and their spread:

   | case | run 1 | run 2 | spread | allocs/op | bytes/op |
   |---|---|---|---|---|---|
   | write rows f32 | 4.473 | 4.625 | 3.4% | 18,013 | 3,507,816 |
   | write rows base64 | 1.966 | 2.803 | 42.6% | 19,051 | 3,809,101 |
   | write columns | 5.268 | 5.199 | 1.3% | 20,562 | 5,147,029 |
   | write text-only | 1.173 | 1.173 | 0.1% | 13,017 | 2,629,161 |
   | bm25 | 0.576 | 0.572 | 0.7% | 3,719 | 990,097 |
   | filter+order | 3.827 | 3.877 | 1.3% | 45,923 | 8,142,905 |
   | bm25+filter | 2.302 | 2.373 | 3.1% | 28,433 | 6,980,217 |
   | sparse | 0.477 | 0.509 | 6.6% | 3,633 | 888,716 |
   | maxsim | 2.520 | 2.109 | 16.3% | 27,541 | 2,026,451 |
   | dense ann | 0.855 | 0.977 | 14.3% | 3,609 | 969,921 |
   | multi rrf | 1.449 | 1.637 | 13.0% | 5,804 | 2,002,210 |
   | compute dist | 0.906 | 1.037 | 14.5% | 3,980 | 1,033,442 |
   | highlight | 0.564 | 0.636 | 12.8% | 3,916 | 1,011,022 |

   Time spreads up to 43% between runs on this host, so a time threshold would be noise.
   Allocation counts and digests did not move at all, so those two are the reproducible
   measures here. Findings for later work, not acted on here: base64 vectors parse in under
   half the time of JSON arrays; columns cost more than rows (5.2 against 4.5 ms, 14% more
   allocations); `filter+order` allocates 12 times what an unfiltered BM25 query does.
4. `cargo test -p pstore-server --test microbench`: 1 ignored. The benchmark lives in
   `tests/`, which cargo-mutants does not mutate, and `gates.sh` does not run it.
