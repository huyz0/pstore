# M15 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

⚠️ **The gate checks M15.1's criteria only**, as for M14. M15.2's lines below restart at 1.

⚠️ **Observed red.** M15.1's first tests were seen failing, with the operators refused as
unknown, as its commit (`90704f6`) records. Every other claim below is by **hand mutation**:
the line was replaced, the named tests were run, and they failed. That is each mutation in
the spec's test plan, plus four more.

## M15.1 — the filters, evaluated per row

The server tests are in `cargo test -p pstore-server --test patterns`.

1. `each_pattern_filter_equals_its_oracle`: over 2,000 strings, against the test's own glob
   matcher, the `regex` crate, and the test's own Levenshtein.
   - It covers each operator and `Not` of each, `id`, an array, a number, and a missing
     attribute.
   - It failed with each of these:
     - the glob's `\z` anchor dropped;
     - `(?s)` dropped;
     - `IGlob` compiled case-sensitive;
     - the band's early exit at a length gap of `k`;
     - the band's last column dropped.
2. `what_a_pattern_cannot_mean_is_refused`: each is `400` naming its operator. It failed with
   the 4,096-byte limit doubled, with `v` allowed 257 characters, and with `max_edits` 3
   accepted.
   - `a_regex_compiles_to_at_most_one_mebibyte` (`cargo test -p pstore-query --lib`) was
     added when a compile limit 64 times larger survived: the test's oversized regex exceeded
     both. It finds where `\w{n}` crosses 1 MiB with the crate's own builder, and a pattern
     is refused from exactly there.
   - Code review noted (n2) that a reversed range such as `[z-a]` is refused, since the
     `regex` crate refuses it. The spec does not list that refusal.
3. `a_pattern_decides_a_delete_by_filter_as_a_query_does`, and `every_predicate_round_trips`
   (`cargo test -p pstore-query --lib`), which failed with the pattern tag changed.
   `two_patterns_are_equal_by_kind_and_source` was added when equality without the kind
   survived.

## M15.2 — the trigram sketch

The server tests are in `cargo test -p pstore-server --test sketch`, and the format's in
`cargo test -p pstore-format --test sketch`.

1. **Sound** — `a_sketch_never_changes_an_answer`: declared and undeclared, over values holding
   `ſ`, `ς`/`Σ`, `µ`/`μ`, U+212A and `İ`.
   - It failed with a regex literal run joined across a non-literal node, and with lowercasing
     in place of the fold. `the_fold_is_simple_case_folding` also failed with lowercasing.
   - `a_fuzzy_match_at_distance_k_is_never_pruned` failed with `3k` as `2k`.
   - `a_glob_class_never_prunes_a_match` came from code review B1.
2. **Prunes** — `a_sketch_prunes_a_selective_pattern`: fewer block bytes, equal answers, equal
   depth. It failed with the sketch not consulted, and with it not written.
3. **Budget.**
   - `a_declaration_never_changes_the_block_layout`: the same blocks at 10 to 20,000 rows.
   - `a_sketch_never_costs_the_open_a_second_read`: at every budget, byte by byte, where the
     undeclared segment fits the suffix read, the declared one fits too. It was added when two
     mutations survived: the sketch sized from the whole budget, and without its directory
     entry. It kills both.
4. **Fixed at creation.**
   - `a_declaration_is_fixed_when_its_index_is_created` failed with `$trgm` ignored.
   - `a_compaction_keeps_the_sketch` (`cargo test -p pstore-engine --test sketch`) came from
     code review M2.
   - `a_trigram_set_round_trips_with_and_without_full_text` (`cargo test -p pstore-engine
     --test head`) failed with M14's count omitted before the trigram section.
5. **Format.**
   - `nothing_declared_writes_what_m14_wrote` failed with a section written when nothing is
     declared.
   - `a_sketch_round_trips_and_a_truncated_one_is_refused`.
6. **Gates** — NOT-RUN yet: M15's sweep follows M14's.

Spec review took two rounds. Code review took two rounds: round 1 blocked on B1 (glob classes
read two ways) with majors M1 (each condition decoded once per fold) and M2 (compaction keeps
the sketch), and round 2 passed.
