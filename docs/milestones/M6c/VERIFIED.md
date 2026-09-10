# M6c — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a **mutation applied to the shipped code**, the named test run, and
the failure read. The commands are `cargo test -p pstore-format --test text_fields`,
`cargo test -p pstore-query --test text_field` and
`cargo test -p pstore-engine --test text_field`.

1. **A segment records the field it indexed** — `a_segment_records_the_text_field_it_indexed`,
   which checks `text_fields()` *and* that `Section::TextFields` is present, so the answer
   cannot be coming from the fallback. ⚠️ It asserts the **default** case too: `"text"` is
   recorded like any other name. A writer that skipped the section for the default would make
   absence mean two things in a newly written segment, and would leave the common path — the
   only path anything runs today — carrying no test at all.
2. **Absence is the old default, not "no text field"** —
   `a_segment_without_the_section_reads_as_the_old_default` (format) and
   `a_pre_m6c_segment_still_answers_the_default` (query, end to end through `query()`).
   Both fixtures are built through the writer with `without_text_fields_section_for_test`
   rather than byte-patched: a hand-edited directory is a fixture testing the fixture.
   Observed red by returning `Vec::new()` for the absent case — which refuses every text query
   against every segment written before this milestone, which is all of them.
3. **A segment with no text index pays nothing** —
   `a_segment_without_text_writes_no_text_fields_section`: no section, and `text_fields()`
   empty rather than `["text"]`. The writer gates on `TextPostings` being present, not on the
   caller having passed a name, so a name without postings cannot claim an index that is not
   there.
   ⚠️ **`a_name_without_postings_writes_no_section` exists because the mutation gate said the
   first test was not testing the guard.** With no name passed, `self.text_fields.is_empty()`
   short-circuits and `has_text` never runs — so `*s == Section::TextPostings` survived
   `== -> !=` **and** `&& -> ||`. The second fixture passes a name, attaches no postings, and
   attaches a **`Fieldnorms`** section: without that second, non-text section the mutants are
   indistinguishable from the original. Both observed red against the shipped code, then
   confirmed caught by a re-sweep.
4. **Three meta-region entries each resolve to their own bytes** —
   `three_meta_entries_still_resolve_to_their_own_bytes`: two named vector fields force
   `Fields`, the text index forces `TextFields`, and the block index is always there. All three
   read back, and the segment still scans.
   ⚠️ **Observed red**, and this is the finding that made the patch loop a task rather than a
   line. The loop special-cased "one entry or two" with `patched == 2 && n == 0`; reinstating
   that accumulation with three entries hands the block index the `Fields` table's offset, and
   the segment decodes **without complaint**. Its own comment says a wrong slot here "corrupts
   silently rather than fails", which is why a third hardcoded arm was never the fix.
5. **The named attribute is the one indexed** — `a_named_attribute_is_the_one_indexed`: a
   `body` corpus folded through `Engine::with_text_field("body")` produces a segment with
   postings, a `TextFields` of `["body"]`, and its term dictionary sidecar beside it.
   ⚠️ It asserts the **negative** in the same test: the same corpus through a default handle
   produces no text index at all. Without that half, a `seal` that ignored its argument
   entirely passes. Observed red by handing `text::build` the constant — which fails all three
   tests in the engine file, because it is the milestone.
6. **A query is checked against the segment, in both directions** —
   `a_query_naming_the_wrong_text_field_is_refused` and
   `a_segment_built_over_a_named_attribute_answers_that_name`.
   ⚠️ The direction that matters is `text` against a `body` segment. Before M6c.2 it was
   **accepted** and answered with `body`'s ranking under the name `text` — a confident wrong
   answer, which is worse than an empty one and is exactly what `run.rs`'s original comment
   said the check existed to prevent. Observed red twice: reverting the comparison to
   `DEFAULT_TEXT_FIELD`, and separately dropping the recording in `build_all`. Each turns both
   tests red.
7. **A merge keeps the field it merged** — `a_compaction_does_not_rebuild_over_the_wrong_field`
   and `a_compaction_of_disagreeing_inputs_is_refused`.
   ⚠️ **The destructive half.** `seal` is reached by `fold` and by `compact`, and a compaction
   re-analyzes the original attribute — postings cannot be inverted back into text. A handle on
   the default merging a `body` index rebuilds it over `"text"`, finds no strings, and writes
   no postings: the index is gone, all 24 rows are still there, and nothing errors. Observed
   red by sealing with `&self.text_field`. The disagreement arm was observed red by taking the
   first name instead of refusing, and the test also asserts the refused compaction **did not
   commit**.
8. **Gates** — `./scripts/gates.sh` green (99 suites), `scripts/ndcg.sh` **NDCG@10 1.0000,
   MRR 1.0000** against floors of 0.80 / 0.75 with the control at 0.5441, `scripts/depth.sh`
   **depth 1 + 1** and byte-exact at 20,000 rows, workspace regions **95.37%** via
   `./scripts/coverage.sh --fail-under-regions 95` (`pstore-format/src/writer.rs` 99.02%,
   `pstore-query/src/run.rs` 95.81%). Mutation, two sweeps in the `dev` container:
   `--file crates/pstore-format/src/writer.rs crates/pstore-query/src/run.rs` — **89 mutants,
   76 caught, 2 missed, 11 unviable**; and the M6c functions across every changed crate
   (`--check 'seal_segment|decode_text_fields|Segment::text_fields|Engine<S>::seal|Engine<S>::compact|with_text_field'`)
   — **97 mutants, 85 caught, 8 missed, 4 unviable**.
   ⚠️ **Every one of the ten misses is code this milestone did not touch**, and they are
   backlog item 11 rather than a footnote: the INDEX_BUDGET constant's subtraction; `writer.rs:214`'s
   `*s == Section::SparsePostings && !b.is_empty()` from M3b (`3d7cd14`) — the same guard
   shape M6c's had, with the same untested discrimination; `Engine<S>::compact:811`'s retry
   ceiling from M2.8 (`76091fb`), four mutants including `-> true`, because nothing drives a
   compaction to it; the commit_stale_for_test helper; one in `pstore-node`; two in
   `pstore-testkit`.

## What is not built, and named rather than omitted

- **The schema.** Nothing tenant-facing sets the field, and there is no way to change it on an
  index that already has segments. Those are write-side policy questions and they need the
  server that does not exist. M5c's placeholder is now half-replaced, and the half that is
  replaced is the one where being wrong is silent.
- **More than one text field per segment.** The table holds a list and the code writes one. A
  second would also need its own `Fieldnorms` section, which this does not build — the point
  is that it no longer needs a format change.
- **A refusal for a document whose named attribute is not a string.** Considered in spec review
  and rejected with the reasoning in the spec: `check_storable` takes no field name, and a
  refusal conditional on other documents in the same batch would accept a document in one write
  and refuse the identical one in another.
- **A tenant's segments may now disagree**, and a fan-out across them would refuse against some
  and answer from others. Nothing can produce that today. It is the shape the schema milestone
  has to resolve, recorded rather than left to be discovered.
