# M14 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

⚠️ **The gate checks M14.1's criteria only**, as for M11 and M13. M14.2's lines below restart at 1.

## M14.1 — analyzer options

Server tests are in `cargo test -p pstore-server --test analyzer`. **Observed red:** all eight
first-written tests failed on the pre-M14 tree (`c624be4`):
- six because a write declaring `full_text_search` was refused;
- one because the index's stats carried no analyzer;
- one because the refusal named no rule.

The format tests failed to compile, since `Analyzer` did not exist, and the engine's
compaction test did not compile either. Each hand mutation below was run by replacing the
line and running the named test.

1. **The default is today.**
   - `the_default_analyzer_is_the_pre_m14_split` (`cargo test -p pstore-format --test analyzer`)
     over mixed-script strings.
   - `an_undeclared_index_answers_as_before_m14`: the same ids, and scores within `1e-6` of
     numbers measured on the pre-M14 tree (c 1.286361, a 1.1787057, e 0.38774547,
     b 0.31241912).
2. **Each option applies**, before and after the fold —
   `each_option_applies_before_and_after_the_fold`: stemming, stopwords, case, folding, and
   German.
   - It failed when the query was analyzed with the default, and when the fresh segment was.
   - `each_step_runs_in_its_order` pins the step order and the `case_sensitive` interplay.
3. **`k1` and `b`** — `k1_and_b_apply`: the scores equal an independent BM25 within `1e-4` and
   differ from the defaults'. It failed with the constants restored in `search_with`.
4. **Fixed at creation.**
   - `an_analyzer_is_fixed_when_its_index_is_created` covers the schema, unfolded rows, `true`,
     a re-declared default `k1`, and an undeclared write read after the fold. It failed with an
     absent `$fts` read as the default, with the door's declared rung skipped, and with `true`
     writing no `$fts`.
   - `two_writers_creating_an_index_leave_one_analyzer`.
   - `a_declaration_superseded_in_its_creating_fold_still_fixes_the_analyzer` came from code
     review B1. It failed before the fix: `running` found nothing after the fold.
5. **Recorded.**
   - `a_compaction_keeps_the_analyzer` (`cargo test -p pstore-engine --test analyzer`). It
     failed when compaction sealed with the default.
   - `the_analyzer_is_recorded_and_never_shown_as_an_attribute`: a new process, `GET`, and no
     `$fts` in a response. It failed with `$fts` not stripped.
   - `a_past_epoch_answers_with_its_own_analyzer`: drop and recreate. It failed with `as_of`
     using the default.
   - `a_full_text_schema_round_trips_live_and_dropped` (`cargo test -p pstore-engine --test
     head`): a HEAD of defaults is a byte prefix, and a cut inside the section is refused. It
     failed with the section unread.
6. **Refusals** — `what_an_analyzer_cannot_mean_is_refused`.
7. **Gates.** `./scripts/gates.sh` on `3196cb4`: all seventeen PASS.
   - `./scripts/mutants.sh --check . --in-diff` over the source diff `b9030e7..e613bff`,
     swept in a worktree at `e613bff`. That is M14's source plus M13.2's review fixes
     (`97cbb7f`), which M13's close said would be swept with M14: 328 mutants, 173 caught, 149 unviable, 6 missed.
   - Each miss was killed by hand against a test added for it and seen failing:
     - `describe` returning an empty string, and returning `"xyzzy"`:
       `an_analyzer_is_fixed_when_its_index_is_created` now names both analyzers;
     - `prepare`'s `&&` as `||`: `a_patch_never_resurrects_a_row_a_delete_vector_buries`;
     - `fresh_view`'s `==` as `!=`:
       `the_fresh_view_follows_an_analyzer_another_process_recorded`;
     - `declared_fts`'s `+ 0.0` as `- 0.0`: `a_negative_zero_parameter_is_zero`;
     - `declared_fts`'s tokenizer arm deleted: the named `word_v1` is accepted in
       `an_analyzer_is_fixed_when_its_index_is_created`.

## M14.2 — token predicates

Tests are in `cargo test -p pstore-server --test tokens`. **Observed red:** all five
first-written tests failed with "unknown operator".

1. `each_token_predicate_equals_brute_force_under_the_index_analyzer`: 2,000 rows under a
   stemming analyzer, whose answers are asserted to differ from the default's.
   - Each operator is checked alone and under `Not`, `And` and `Or`, over the text field,
     `title`, an array and a missing attribute.
   - It runs on three paths: ordered, ranked, aggregated.
   - It failed with each read bind removed (answer, ordered, aggregate) and with a sequence's
     order ignored.
2. `before_the_first_fold_the_declared_analyzer_applies`, over the same paths.
3. `a_token_predicate_decides_at_the_fold_as_a_query_does`:
   - `delete_by_filter` in the creating fold;
   - `patch_by_filter` of `Not(token)`;
   - a conditional upsert.

   It failed with `condition_of` unbound, and with the creating fold binding the default.
4. `every_predicate_round_trips` (`cargo test -p pstore-query --lib condition`) includes an
   unbound and a bound token predicate.
   `tokens_are_three_valued_and_never_admit_unbound` failed with the unbound check removed,
   and with `Not` of unbound answering.
5. `a_token_predicate_adds_no_round`: filtered depth equals unfiltered, 3 for an ordered
   query. The criterion was amended, because a ranked query is 4 deep with no filter at all.
6. **Gates** — `./scripts/gates.sh` and the sweep, as M14.1's line 7 records for both tasks.

Spec review took two rounds: round 1 had seven majors, and round 2 one major (`Not` of
unbound), which was fixed. Code review took two rounds: round 1 blocked on B1, and round 2
passed.
