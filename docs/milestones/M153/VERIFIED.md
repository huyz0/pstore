# M153 verification

The spec was reviewed in three rounds before any code.
- **Round 1** had four majors: the `doc_count` wording, `b`'s analyzer, a further field under
  `fts` that M152's guard alone would still split, and the request formula.
- **Round 2:** the old `FullText` entry points keep M151's refusal, so criteria 3 and 5 moved to
  the new `_texts` entry points, measured with the ranking-only `query_with_texts`.
- **Round 3 passed.**

One amendment came after the code, from the mutation run below. Criterion 2's fixture had `b`
in one segment only. A segment's df lookup falls back to its own df when the query's
statistics lack a term, and with one carrier those are equal, so two mutations survived. The
oracle also went through `run` over every segment, so it shared the mutated statistics. The
fixture now has a third segment carrying `b`, and each oracle queries only the segments
carrying its field. The spec's criterion 2 says so.

How to run the tests:
- `cargo test -p pstore-query --test it several_fields::`
- `cargo test -p pstore-engine --test it multi_text:: split::`

Red runs: against the parent, the new tests fail to compile, because `Texts` and the `_texts`
entry points were absent. Before the engine passed its `Texts`, `multi_text::` and `split::`'s
multi-field queries were refused with `Unimplemented("text legs over different fields")`. A
compile failure shows only that the API is new, so each test's power to fail rests on the named
mutations below. They were applied one at a time by a script, each test run, and then
reverted. All 11 were observed red, the two noted above after the fixture change, and one again against the engine test. No
`cargo mutants` run is claimed for them.

1. `several_fields_fuse_as_their_own_rankings_do` (`crates/pstore-engine/tests/it/multi_text.rs`):
   green after. It checks Sum, Max and RRF before the first fold, after it, beside fresh rows,
   after a compaction and `as_of` the first fold. The single-field point is M152's
   `each_further_field_scores_as_its_own_index_would`, whose `same` now compares score bits
   rather than to 1e-6, green after. Killed:
   - "every leg scores under `default`";
   - "every leg reads the first field's statistics";
   - "the fresh view's texts are not passed";
   - "`as_of` passes no texts".
2. `each_field_is_scored_by_its_own_statistics` (`crates/pstore-query/tests/it/several_fields.rs`):
   green after. Killed:
   - "the statistics are summed over every segment, carried or not";
   - "a field's terms are analyzed under `default`".
3. `what_several_fields_cannot_mean_is_refused` (the same file) and, through the engine,
   `an_unknown_field_beside_a_known_one_is_refused` (`multi_text.rs`, added after code review
   round 1 found the engine half untested): green after. Its second point,
   the old entry points' refusal, is M151's and was green on the parent. `open_part` and
   `part` keep M151's tests. Killed:
   - "a field no segment carries answers nothing", by each of the two tests;
   - "`run` accepts a phased share beside several fields".
4. `a_query_over_several_text_fields_is_not_split` (`split.rs`): green after. Killed: "a
   multi-field query is split", "a primary query is not split".
5. `two_fields_cost_one_open` (`several_fields.rs`): green after. Killed: "each field reopens
   the sidecar". One finding while writing it: the sidecar is a whole-object GET, which byte
   accounting does not record, so its single fetch is held by the request count's `- R(open)`
   rather than by bytes. The test says so.

The whole workspace's tests pass, except the two IPv6 `peer_http::response_tests`, which fail
in this container on the parent too. In that run `pstore-server`'s
`ref_new::adversarial_writers_never_both_win` failed once, at an outcome with no `upserted`
count. It writes no text and touches nothing this milestone changed. It passed in three
reruns of `ref_new::` alone, so it is recorded as a flake under load, not fixed here.

Code review round 1 passed with no blocking or major findings. Its minors:
- criterion 3's engine half had no test; `an_unknown_field_beside_a_known_one_is_refused` now
  covers it;
- `two_fields_cost_one_open`'s sidecar-bytes assertion cannot fail, since whole GETs are not
  byte-accounted. The request count holds the single fetch, as item 5 says;
- `several_agree` does not assert criterion 1's precondition that no two expected top-k scores
  are within 1e-5. A breach could make it flaky, never pass wrong code.
