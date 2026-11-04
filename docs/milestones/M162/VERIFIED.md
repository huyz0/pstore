# M162 verification

Implemented in `crates/pstore-engine/src/recall.rs` (`Engine::recall`, the probe, the
budget), `crates/pstore-engine/src/lib.rs` (`Engine::query_head`, factored out of
`query_as_of_filtered`, which now calls it, and the two refusals),
`crates/pstore-server/src/lib.rs` (the route, its validation and status mapping) and
`src/types.rs` (`RecallResponse`). All runs are in a 4-core cloud container, and every
number is `provisional`.

How to run: `cargo test -p pstore-engine --test it recall::` and
`cargo test -p pstore-server --test it recall:: replication::a_replica_has_no_recall`.

Red runs: the engine tests were written against an `Engine::recall` stub that answered
`None`, and all five failed (.harness/m162-red-engine.log); the server's `recall_route` and
`a_replica_has_no_recall` failed against a route that did not exist yet
(.harness/m162-red-server.log).

Mutations, applied one at a time by script and reverted, each running the engine and
server tests named above (.harness/m162-mutations.log, .harness/m162-mutations-r2.log).
The script reports a compile error as invalid, not as a kill, and none was invalid. The
first run's two survivors, the probe search made exact and the server's default seed
changed, were killed in the second run after the tests compared the sampled queries with
a probe search of their own and the default seed's ground truth with seed 0's. All 29
were killed, among them: the budget unchecked, strict, counted once with a filter or per
request rather than per query; either search's exactness or filter swapped or dropped;
the probe outside [-1, 1) or ignoring its seed; the replica allowed; the dimension check
dropped; the vectors flag ignored; `num` reported as asked; every query given a share;
and each server default and range moved.

No `cargo mutants` run is claimed.

CI's mutation shard 5 found `z >> 31` → `z << 31` in `recall_probe` missed: the probe
test checked only the draws' range and spread. `a_probe_is_its_seeds` now pins seed 0's
first three draws to splitmix64's published outputs; run here against that mutant it
failed, and passed once the line was restored.
Shard 4 then found `z ^= z >> 31` → `z |= z >> 31` missed. That is an equivalent
mutant: the step changes only bits below the 16 the probe keeps, so it was removed. The
pinned draws pass unchanged without it, which shows the probe's values did not move.

1. PASS: `recall_matches_its_own_queries`. Over 2,000 documents folded with lists of
   about 10 rows, the three averages equal the test's own default and `exact` queries of
   each sampled vector, and `avg_recall` is below 1. The sampled vectors are the probe
   search's, in its order. Over 50 documents, scanned exactly, `avg_recall` is 1.
2. PASS: `recall_is_reproducible`. The same seed twice gives equal responses, another
   seed other ground truth, every `query_vector` is a written document's vector, and
   three documents sample three queries. `recall_route` checks the same over HTTP and
   that the default seed is 0.
3. PASS: `recall_with_filters`. With `even = true` the averages equal the test's
   filtered queries and every neighbour is even; a filter no row satisfies gives
   `avg_recall` null, both counts 0 and `num` 6. `recall_route` checks the filter over
   HTTP.
4. PASS: `recall_is_one_snapshot`. Rows flushed and not folded leave the response
   unchanged; after the fold `epoch` is the fold's and above the earlier one.
5. PASS: `recall_route` refuses `num` 0 and 26, `top_k` 0 and 101, `seed` -1 and 1.5, an
   unknown field and a bad filter with 400, and an absent index with 404.
   `recall_refusals` refuses past the budget, and past it only because of a filter's
   doubling, with `RecallTooLarge` after one HEAD read each, and an index with no
   default vector dimensions with `RecallNeedsVectors` after one HEAD read. The unit test
   `recall_refusals_are_named` maps them to `400 recall_too_large` and
   `400 recall_needs_vectors`. `a_replica_has_no_recall` gives `409 replica_no_history`.
6. PASS: `recall_is_one_snapshot` reads HEAD once from a fresh process, with no LIST and
   no write; `recall_route` reports `cost` with no LIST and no write.
7. PASS, with the IPv6 exception M127 records.
   - `cargo fmt --all --check` and `cargo clippy --all-targets --all-features -- -D
     warnings` passed (.harness/m162-clippy.log).
   - The workspace suite passed 2,385 tests with none failing
     (.harness/m162-quality-suite.log). It ran with all features and both `[::1]` tests
     skipped: the container has no IPv6, and those tests run in CI.
   - `./scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` passed at
     97.19% lines and 95.76% regions (.harness/m162-coverage.log).
   - `scripts/recall.sh`, `scripts/ndcg.sh` and `scripts/depth.sh` passed
     (.harness/m162-quality.log). `check-links` failed in that run only on the links to
     this file, which did not exist yet; it was run again after this file was written.

Code review: see the commit message.
