# M160 verification

Implemented in `crates/pstore-engine/src/bundle.rs` (`directory_from_tail`, which reads a
bundle's directory from a tail read against the bundle's length, and `directory`, the
parser `read_index` now shares), `crates/pstore-engine/src/lib.rs` (`Engine::progress`,
`Progress` and `PROGRESS_BUNDLES`) and `crates/pstore-server/src/lib.rs` with
`src/types.rs` (the `progress` query parameter and the summary's `progress` object). All
runs are in a 4-core cloud container, and every number is `provisional`.

How to run: `cargo test -p pstore-engine --lib bundle::`,
`cargo test -p pstore-engine --test it progress::` and
`cargo test -p pstore-server --test it progress::`.

Where the code differs from the spec's words:
- The summary's field is `progress`, not `index`: the summary already has `"index"` for
  the index's name. The spec was corrected before the code was written.
- `Progress` also carries `held`, whether HEAD holds the index. Only the engine tests read
  it; the server answers 200 for a name HEAD does not hold from `unindexed_rows > 0`.
- The spec's lane cap (`exact` false past 64 present lanes) needs no line of its own: with
  64 lanes read, each lane gets no forward probe, and a lane with no probe is marked
  inexact like one whose every probe answered present. Mutation found the separate check
  redundant, and it was removed.

Red run (.harness/m160-red-bundle.log): `a_directory_reads_from_the_tail` was written
before `directory_from_tail` was correct and failed against it, its first assertion
answered `CorruptBundle`. The engine and server tests
were written after the code they call existed, so their red is the mutations below.

Mutations, applied one at a time by script and reverted, each running the `bundle::` unit
tests, the engine `progress::` tests and the server `progress::` test. The script reports
a compile error as invalid, not as a kill, and none was invalid. The first run
(.harness/m160-mutations.log) left four survivors; tests were added for each and the
runs after them (.harness/m160-mutations-r2.log, .harness/m160-mutations-r3.log; the
second was stopped by its time limit after 13 mutants and the third ran the rest) killed
all 22:
- the tail reader taking the directory's offset from the buffer, never asking for a long
  directory, not checking the length against the footer, or not checking either magic
  (`a_directory_reads_from_the_tail` now corrupts each);
- the probe starting at 0 rather than the watermark, reading past the lane cap
  (`progress_reads_the_lowest_lanes_past_the_budget`), probing one key too many, ignoring
  a lane whose every probe answered, counting past a gap
  (`progress_stops_a_lane_at_its_first_gap`), dropping the first bundle, reading whole
  bundles rather than a tail (`progress_reads_only_a_bundles_tail`), not summing bytes, or
  reporting every name held;
- the summary always adding progress, accepting `maybe`, answering 404 for a durable name,
  answering 200 for any name, always `updating`, or a wrong `as_of_epoch`.

No `cargo mutants` run is claimed.

1. PASS: `summary_progress`. Without the parameter and with `progress=false` the summary
   has no `progress` field and costs one blob read. Every existing summary test runs
   unmodified in the suite below.
2. PASS: `progress_counts_other_lanes`, `summary_progress`. After a second engine (and a
   second `Api`) on another lane makes two bundles durable, before any fold: `idx`'s rows
   are its two upserts and one delete, its bytes the sum of its entry lengths read from
   the bundles by hand, `exact` true, and the rows of `other` in the same bundle are not
   counted. An index no fold has made (`fresh`) counts its own, with `held` false; the
   server answers it 200 with 0 segments and 0 documents only with `progress=true`.
3. PASS: `progress_counts_other_lanes`, `summary_progress`. After a fold: 0 and 0,
   `up-to-date`, and `as_of_epoch` the fold's epoch.
4. PASS: `progress_is_bounded`. 65 bundles on one lane: 64 counted, `exact` false. With a
   second present lane, 1 + 31 of the first lane's and the second's one. Under the budget,
   31 bundles on each of two lanes count 62 with `exact` true, and a 32nd makes it false.
5. PASS: `a_directory_reads_from_the_tail`. A directory of 40 long names past a 256-byte
   tail answers the range ending at the footer, and that range decodes to the same
   directory as a whole-bundle read.
6. PASS: `progress_costs`, `progress_reads_only_a_bundles_tail`. An idle tenant with three
   lanes costs 5 reads (HEAD, the registry, three probes); two bundles on one lane cost
   2 + 3 + 63 + 2. No write and no LIST. A 2,000-row bundle is read as one 4 KiB tail.
7. PASS: `summary_progress`. `progress=maybe` is 400, and an absent index is 404 with and
   without the parameter.
8. PASS, with the IPv6 exception M127 records and one unrelated flaky test.
   - `cargo clippy --all-targets --all-features -- -D warnings` passed
     (.harness/m160-clippy.log). `cargo fmt --all --check` failed on two lines of
     `tests/it/progress.rs`; `cargo fmt` fixed them, the check then passed, and the
     engine `progress::` tests and the engine's clippy passed again after it.
   - The workspace suite ran with all features and both `[::1]` tests skipped (the
     container has no IPv6; those tests run in CI): 2,371 passed and 1 failed
     (.harness/m160-quality-suite.log). The failure is
     `parallel_masks::a_dropped_query_aborts_its_masks` in `pstore-query`, which M160 does
     not touch: "no mask read began" inside a 100 ms window, with the machine loaded. It
     passed in the coverage run below, and `parallel_masks::` passed five times in a row
     when run alone.
   - `./scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` passed at
     97.19% lines and 95.76% regions (.harness/m160-coverage.log).
   - `scripts/recall.sh`, `scripts/ndcg.sh` and `scripts/depth.sh` passed
     (.harness/m160-quality.log), and so did `check-poison`, `check-links`,
     `build-index --check`, `check-milestone-ids` and `check-portable`
     (.harness/m160-fast.log).

Code review: see the commit message.
