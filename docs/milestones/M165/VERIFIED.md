# M165 verification

Implemented in `crates/pstore-query/src/explain.rs` (the collector), `src/run.rs` (the
strategy a dense leg returns, recording in `candidates` and `run`), `src/fuse.rs`
(`Fusion::name`), `crates/pstore-index/src/vec_index.rs` and `src/fde_clusters.rs` (the
probes a search made), `crates/pstore-engine/src/lib.rs` (the fresh ordinal) and
`crates/pstore-server/src/explain.rs`, `src/lib.rs` and `src/types.rs` (the flag,
`meta.explain` and the refusals). All runs are in a 4-core cloud container, and every
number is `provisional`.

How to run: `cargo test -p pstore-query --test it explain::`,
`cargo test -p pstore-engine --test it explain` and
`cargo test -p pstore-server --test it explain`.

Red runs: the query tests failed against a collector that recorded nothing
(.harness/m165-red-query.log), the engine's `explain_counts_shadow_and_excludes_the_fresh_segment`
before the engine named its fresh segment (.harness/m165-red-engine.log), and the server's
`explain_reports_the_plan` and `explain_counts_remote_segments` before the response carried
the plan (.harness/m165-red-server.log). `explain_keeps_only_the_last_attempt` and
`a_query_not_collected_records_nothing_and_a_collection_reports_its_last_run` failed with
the reset removed (.harness/m165-reset-mutation.log). `explain_refusals` passed in the red
run; each of its three refusals was seen red by mutation below.

Mutations, applied one at a time by script and reverted, each running the three commands
above (.harness/m165-mutations.log). The script reports a compile error or a pattern that
does not match as invalid, not as a kill. The first run had one survivor, the named
field's probes not capped at its lists, and two invalid patterns; after
`explain_reports_a_named_fields_probes` was added and the patterns corrected, all 28 were
killed, among them: a reason swapped, widening dropped or recorded at any ask, the dense
strategy taken from the request, probes uncapped on either path, a failed share counted as
remote and a remote one not counted, `kept` taken before the mask, the fresh segment
counted, an attempt not reset, the fusion misnamed, the plan rendered unasked, and each
refusal dropped.

No `cargo mutants` run is claimed.

1. PASS: `explain_reports_the_plan` over two clustered segments of 10 lists reports
   `limited`, 2 approximate and 0 exact segments, 16 probes, `requested` 20, `kept` 20 and
   `shadow` 0; with `exact: true`, 0 approximate, 2 exact and 0 probes.
   `explain_records_each_segment_leg` checks the same against segments it built, one of
   them below the threshold and scanned exactly. `explain_reports_a_named_fields_probes`
   checks a named field's clusters: 1 probe at `p` 1, the field's lists at any larger `p`,
   and none when exact.
2. PASS: `explain_records_each_segment_leg` with a filter: every leg `exhaustive` because
   of the filter, `requested` the index rows, `kept` the admitted hits up to each
   segment's room; under `sum`, every leg because of `sum`. `explain_reports_the_plan`
   checks the filter over HTTP.
3. PASS: `explain_records_each_segment_leg` with three rows in a segment's delete vector:
   the dense leg is widened and `requested` is 30 + 3.
   `explain_counts_shadow_and_excludes_the_fresh_segment` and `explain_reports_the_plan`
   check `shadow` and that the fresh segment is in no figure;
   `the_fresh_segment_is_in_no_figure` checks it at the query level.
4. PASS: `explain_records_each_segment_leg` with two text legs around a dense one, in
   request order, each text leg's `candidates` the BM25 matches up to each segment's ask.
5. PASS: `explain_counts_remote_segments` across two servers counts the other's segments
   as remote and only the local ones in `legs`; with the other failing, `remote_segments`
   is 0 and `legs` counts every segment.
6. PASS: `explain_reports_the_plan` compares `results` and `meta.cost` with and without
   explain, and finds no `explain` key without it or with `explain: false`.
7. PASS: `explain_refusals` answers 400 for `explain` as a string, with `rank_by`, with
   `aggregate_by` and inside a multi-query.
8. PASS: `explain_keeps_only_the_last_attempt` reaps the cached HEAD's segments, so the
   first attempt fails and the retry answers; the plan is the retry's alone.
9. PASS, with the IPv6 exception M127 records.
   - `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` passed.
   - The workspace suite passed 2,546 tests with none failing (.harness/m165-suite.log),
     with both `[::1]` tests skipped: the container has no IPv6, and those tests run in CI.
   - `./scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` passed at
     97.29% lines and 95.94% regions (.harness/m165-coverage.log).
   - Replayed onto main after M162 and M179 landed. The multi-query parser conflicted with
     M179's refusal of projected vectors, and both refusals were kept. On that tree, fmt and
     clippy passed again, and the suite passed 2,600 tests with none failing, the same
     two `[::1]` tests skipped. Coverage was not re-run there.
   - CI's mutation shard 9 found three mutants missed in `run.rs`: the fresh ordinal's
     bound (`<` to `<=`), and a phased share's remote count (`+=` to `-=` and to `*=`).
     `a_fresh_ordinal_past_the_targets_names_none` and a text query in
     `explain_counts_remote_segments` now catch them. Each was run here against its mutant
     and failed, and passed with the line restored.
   - CI's mutation shard 0 found `>` to `>=` missed where the engine names its fresh
     ordinal. It is equivalent: an ordinal past the targets names none, which
     `a_fresh_ordinal_past_the_targets_names_none` pins, so the comparison was removed and
     the engine passes `Some(unfolded_at)`. With that change the engine's tests (607 in
     `it`) and the explain tests in pstore-query and pstore-server passed here, with fmt
     and clippy.

Code review: see the commit message.
