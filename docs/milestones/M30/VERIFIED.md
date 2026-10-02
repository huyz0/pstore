# M30 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0), against
`MemoryStore`. No number here is a latency.

Commands: `cargo test -p pstore-engine --test schema`, and `-p pstore-query --test text_field
--test hybrid`. Red on the spec's commit (`8fafcd1`) unless a criterion says otherwise.

1. **The probe's sequence ends compactable.** `an_empty_text_field_is_filled_by_the_first_fold_with_text`.
   Parent: the schema stayed `""`. Killed: the fill dropped.
2. **A patch that adds text fills.** `a_patch_that_adds_text_fills_the_text_field`. Parent: `""`.
   Killed: the fill dropped, and the fill on any fold.
3. **A fold over another field indexes the schema's.** `a_fold_indexes_the_schemas_text_field_not_its_own`.
   Parent: no `body` postings. Killed: the fold sealing over its own field.
4. **The fresh segment agrees.** `the_fresh_segment_indexes_the_schemas_text_field`, and
   `a_cached_fresh_segment_is_rebuilt_when_the_text_field_is_filled`.
   - The second test was added for a hand-mutation survivor: the cache key ignoring the field.
   - Killed: the fresh segment built over its own field, and that key.
5. **A compaction uses the schema's field.** `a_compaction_rebuilds_over_the_schemas_text_field` and
   `a_compaction_refuses_an_input_of_another_field`. Parent: the merge was over the default
   field, and it committed. Killed: the compaction ignoring the schema.
   - Code review found that a compaction with no field recorded and none named still defaulted.
     A single process could then make the index uncompactable.
   - `a_compaction_with_no_field_to_keep_builds_no_text_index` was red before that fix.
     Killed: the default restored.
6. **Vector-only folds fill nothing.** `a_vector_only_fold_fills_no_text_field`. It passes on the
   parent, as it should, and was seen red under "fill on any fold".
   `before_any_text_the_fresh_segment_indexes_the_process_field` pins the fallback while
   `""`; killed: the empty-field filter dropped.
7. **A vector-only segment does not refuse a text query.** `a_vector_only_segment_does_not_refuse_a_text_query`
   and `a_segment_with_no_text_index_answers_nothing`.
   - Parent: `Query("no such vector field")`, and the same sequence in a probe before the
     test. Killed: the exemption removed.
   - `a_text_leg_is_no_longer_refused_and_still_names_what_is_missing` changed with the
     contract, as the spec records, and still asserts the missing-sidecar error.
   - `a_query_naming_the_wrong_text_field_is_refused` passes unchanged.
8. **Gates.**
   - `./scripts/gates.sh`: 17 of 17 by the pre-commit hook on `ee38b9a`, and on this ledger's
     commit.
   - The sweep over M30's source diff (`8fafcd1..ee38b9a`), against those four test files:
     17 mutants, 8 caught, 7 unviable, 2 timeouts, **0 missed**.
     - The timeouts replace the compaction's attempt loop whole, as in M29.
     - By hand: 10 mutations, all killed, after the two tests named above were added.
   - ⚠️ Not `./scripts/mutants.sh` itself: `cargo mutants --in-diff` was run directly.

**Residue (code review, minor):** a text leg naming a misspelled field, over an index whose
segments are all vector-only, now answers empty instead of `UnknownField`. Once any segment
has text it is refused again.

**Not repaired:** an index that already holds segments of two text fields stays uncompactable.
M30 stops new ones.

Spec review took three rounds, the third for the amendment found at implementation. Code review
took two rounds: round 1 blocked on the compaction default, and round 2 passed.
