# M232 verification

How to run the tests: `cargo test -p pstore-engine --test it aggregate_stats` and
`cargo test -p pstore-format --lib aggregates`. The benchmark: `./scripts/columns.sh` and
`./scripts/columns.sh 100000`.

Red runs: the tests name `with_block_options` and `pstore_format::aggregates`, which the parent
does not have, so they were red by not compiling. The mutations below show what each tests.
The first mutation run found criterion 1's test vacuous: its one spec named attributes the
section does not cover, so no block was ever answered from statistics and 3 of 3 mutations
survived. It now runs covered-only specs over narrow zones as well, and rows held unfolded that
replace folded ones.

Mutations were applied one at a time by a script and reverted. 12 of 12 were killed:
`must_admit`'s `Lt` as `Lte`; its `Eq` admitting when either edge equals (killed once `c`, one
value a block, was added after review); its `Gte` as "any zone"; its "every row holds it" check dropped;
its `Or` admitting always; `Count(attr)` counting rows; a block with a deleted row answered; an
answered block also fetched; `Min`/`Max` extremes dropped; a grouped spec answered; a shadowed
segment answered; the mixed-kind check dropped at build. No `cargo mutants` run is claimed.

1. `block_statistics_answer_what_the_rows_do`: green. 300 random filters plus 6 fixed, each
   under four specs (covered attributes, every attribute, `q`, `sparse`) and grouped by `s`,
   with and without rows held unfolded; deletes in two of three segments.
2. `a_filter_admitting_blocks_whole_reads_fewer_bytes`: green (under half the bytes, for an
   unfiltered sum and a sum under `q >= -10`).
3. `single_kind_numbers_are_counted_and_summed_per_block`,
   `mixed_kinds_overflow_and_non_finite_sums_are_left_out`,
   `the_budget_keeps_the_first_names_that_fit`, `a_truncated_or_malformed_section_is_refused`:
   green.
4. `./scripts/columns.sh`, `provisional` (in-memory store, 1 MiB coalescing gap, one 4-core
   container, medians of 31 runs). M231 (columnar, no statistics) against M232:

   | | 20,000 rows: M231 / M232 | 100,000 rows: M231 / M232 |
   |---|---|---|
   | segment bytes | 1,547,072 / 1,548,079 (+1,007) | 7,366,623 / 7,367,846 (+1,223) |
   | meta region | 6,825 / 7,832 | 6,759 / 7,982 |
   | write | 72 / 71 ms | 514 / 561 ms |
   | `sum(price)` | 2 GETs, 1,388,397 B, 12.30 ms / 1 GET, 8,192 B, 0.022 ms | 6,568,014 B, 72.1 ms / 8,192 B, 0.024 ms |
   | `avg(rating) where price >= 1` | 1,388,397 B, 12.78 ms / 8,192 B, 0.023 ms | 6,568,014 B, 73.1 ms / 8,192 B, 0.025 ms |
   | `max(price) where price >= 5001` (no block whole) | 12.39 / 12.32 ms, same bytes | 74.4 / 75.0 ms, same bytes |
   | `count where category =` (no statistics apply) | 12.29 / 12.26 ms, same bytes | 70.6 / 73.4 ms, same bytes |

   The 8,192 bytes are the open's suffix read, which every query pays. No aggregation's bytes
   rose, and the two that cannot use the section stayed within run-to-run noise (+1% to +4%,
   inside the 10% bound). Every mask's bytes were identical; mask times within noise but one
   100,000-row outlier (`customer =` one batch, 0.98 against 1.68 ms, same bytes and blocks).
   Adopted, default on, by criterion 4's rule.
5. `aggregate::a_refused_strong_aggregation_is_not_run_twice` pins `block_aggregates: false`
   and keeps its count of 5 reads (with statistics it reads 4: the block is answered at open);
   `typed_values::a_segment_without_floats_keeps_its_bytes` writes without the section and
   keeps its recorded hash (its int column earned one).

Downsides, measured: (a) **float rounding**: `avg(rating)` answers `2.491587` from block sums
and `2.4915869999999813` row by row at 20,000 rows (`2.4983976999999995` against
`2.498397699999971` at 100,000): totals differ in the last bits, as two segments' partials
already do. (b) **Suffix budget**: the section took 1,007 and 1,223 bytes of the 8 KiB suffix,
left after the sketch and the equality section; at many numeric attributes the last names are
dropped. (c) **Write CPU**: +0 to +9% across scales (one pass a covered attribute). (d) **Rows
held unfolded** that shadow folded ones turn the fast path off for that process's reads of the
index until they fold. (e) Only ungrouped aggregations, and filters `must_admit` can prove,
gain. Rollback: a reader before M232 ignores the unknown section id, so it costs nothing.

Gates, in a cloud container: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` (the two IPv6 peer tests this container cannot run skipped; nothing else failed), `scripts/depth.sh`, `check-links.sh`, `check-verified.py`, `gen-clients.py --check`, `check-slos.py`, `build-index.py --check`, `check-milestone-ids.py`, `check-poison.sh` and `check-portable.sh` passed, on the M231 and M232 tree together. `cargo llvm-cov -p pstore-format`: `aggregates.rs` 97.15% regions, 99.12% lines. The workspace floor is CI's full run.
