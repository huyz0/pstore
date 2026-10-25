# M126 verification

1. `unknown_request_fields_are_refused_before_engine_access`: red before the change for all eleven cases (accepted writes or engine-backed 404 queries), then green with `cargo test -p pstore-server --test closed_requests -- --nocapture`. Includes write/document/query/multi envelopes, repeated-key fallback and escaped names. The ignored-field behavior was directly observed; no mutation-tool run is claimed.
2. `a_refused_batch_leaves_rows_session_and_registry_unchanged`: red before the change (HTTP 200, two durable PUTs), then green with the same command. The test asserts zero request/byte deltas, unchanged rows/session and no new index after refusal.
3. `nested_options_are_closed_but_data_names_are_open`: green on both the preceding implementation and the changed implementation via the same command; these are preserved controls, not newly red regressions. Covers nested schema/patch/query options and valid attribute/vector names/aggregate labels.
4. `the_fast_parse_is_the_tree_parse`: passed via `cargo test -p pstore-server --lib the_fast_parse_is_the_tree_parse -- --nocapture`. The initial workspace run found 24 failures from an invalid replication fixture's ignored root `n`; it now sends attributes. `a_replica_returns_the_written_attributes` was observed red with `n` deliberately omitted, then green after restoring/rebuilding the fixture. All 28 replication tests plus the 3 closed-request tests passed via `cargo test -p pstore-server --test replication --test closed_requests -- --nocapture`. The final `cargo test --workspace --all-features --quiet` passed, including existing API cost/depth tests. The review packet's formatting, Clippy, default tests, links and index checks also passed.
5. `./scripts/parser-bench.sh d1bc45b`: completed in capped Linux Docker on WSL2, Rust 1.90, four CPUs / 8 GiB. [Raw samples](parser-samples.csv) and [measurement source](measure/main.rs) are retained. The harness builds matching production dependency versions and loads the baseline source from Git. It measures typed-then-tree parsing, not HTTP/planning/engine work or error-envelope formatting. No own unsafe code or shipping dependency was added.

## Parser measurement

All timings are **provisional**. Five samples alternate before/after order, with 1000
warmups and 10,000 parses/sample (50,000 for small duplicate-key fallback). Dense
vectors have 768 dimensions; JSON bodies are approximately 12.4 KB. Table times are
sample medians in microseconds. Allocations and allocated bytes are per parse,
measured with `stats_alloc`; byte totals are cumulative allocation, not peak RSS.
Host CPU: Intel Core i5-13600KF; x86_64 Linux Rust 1.90.0 / LLVM 20.1.8. The standalone
measurement profile uses thin LTO and one codegen unit; build jobs are capped at four.

| Case | Before us | After us | Allocations before/after | Reallocations before/after | Allocated bytes before/after |
|---|---:|---:|---:|---:|---:|
| Valid write | 24.468 | 24.772 | 12 / 12 | 8 / 8 | 5618 / 5618 |
| Valid query | 23.959 | 24.109 | 5 / 5 | 8 / 8 | 4299 / 4299 |
| Valid duplicate-key fallback | 0.846 | 0.847 | 20 / 20 | 0 / 0 | 3335 / 3335 |
| Unknown write option | 24.319 | 46.559 | 8 / 25 | 8 / 24 | 4893 / 43306 |
| Unknown document option | 24.683 | 27.949 | 8 / 25 | 8 / 12 | 4970 / 35789 |
| Unknown query option | 23.856 | 27.838 | 5 / 14 | 8 / 16 | 4312 / 34664 |

The old implementation accepted every invalid input; the changed one rejects each.
Valid paths allocate identically. The small timing differences do not establish a
speed change. Refusals allocate more because a typed failure still takes the existing
JSON-tree diagnostic fallback. This measured cost remains explicit; request resource
admission is separate pending work in the [delivery status](../../comparisons/DELIVERY.md).

Independent code review: one completed round, pass with no findings. The normal
pre-commit hook enforces the fast gates before Git creates the commit. No coverage
or mutation floor changed; full coverage/mutation measurements are NOT-RUN for M126.
