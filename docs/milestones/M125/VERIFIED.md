# M125 verification

1. `prefetch_limit_applies_to_the_global_union`: red on current main (two hits
   instead of one), then green via `cargo test -p pstore-query --quiet`.
   It also pins the global tie-break on segment ordinal.
2. `cargo test -p pstore-query --quiet` and `cargo test -p pstore-engine -p pstore-server --quiet` passed, including sum, shadow and distributed paths. The final `cargo test --workspace --all-features --quiet` and completed `bash scripts/gates.sh` also passed after the exhaustion guard.
3. `query_bench`, run by `bash scripts/query-bench.sh`, passed after the query fix, before the final exhausted-write guard: all six non-hybrid digests match the baseline. The hybrid digest changes from `65559801f84a1412` to `9f0e0b2b40a218ae`, because candidates past the requested global limits no longer contribute. Sum remains on its whole-leg path.

## Local measurement

Provisional WSL2, four CPU / 8 GiB container, 24 x 2000 rows, 64 dimensions,
10 warmups and 200 measured queries per kind. Final p50 / p95 milliseconds:

| Query | p50 | p95 |
|---|---:|---:|
| dense | 0.922 | 0.993 |
| text | 0.558 | 0.606 |
| hybrid | 1.063 | 1.144 |
| dense + filter | 4.050 | 4.212 |
| text + filter | 2.137 | 2.250 |
| dense k=100 | 1.961 | 2.047 |
| text k=100 | 1.145 | 1.206 |

These are single runs, not production latency or a statistically established speedup.
Before M125, `bash scripts/depth.sh`, `bash scripts/recall.sh` and `bash scripts/ndcg.sh` passed on the unchanged index code: sparse/text depth 1+1 at 20,000 rows, clustered fast recall@10 0.9810, BM25 NDCG@10 1.0000 versus control 0.5441. The recall harness also reports uniform-data fast recall 0.7380; the passing clustered gate is not a distribution-independent guarantee.

Independent code review: one completed round, no blocking or major findings.
