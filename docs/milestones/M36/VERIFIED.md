# M36 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `MemoryStore`. Every result is a
test outcome; nothing here is timed.

Command: `cargo test -p pstore-engine --test schema`.

⚠️ **A test that fails to compile is not counted as red.** Each criterion names a red on the
parent, or the hand mutation that was seen to fail it.

1. **Any fold quarantines a waived text-field conflict.** `any_fold_quarantines_a_text_field_conflict`:
   one refusal, then the flush writes the row. A `body` engine's fold leaves `a` in the index,
   quarantines `p` with `$text` = `prose` under `reserved`, and counts 1 reject.
   - Red on the parent: the second flush was refused.
   - Killed: the row judged by the folding engine's own field.
   - This is M35's `a_waiver_never_covers_a_text_field`, inverted on purpose.
2. **The sibling race is caught.** `two_writers_text_fields_never_share_an_index`: over a new index
   and over one with an empty text field, the `prose` row is held and found by a `prose` text
   query, and the `body` row is quarantined with its stamp.
   - Red on the parent: the schema recorded `body`, with both rows held.
   - Killed: the stamp dropped at the door, the stamp ignored, and the reject pass judging
     an empty field unfilled.
   - `a_wrong_row_never_chooses_the_text_field` (2b): a wrong-width `prose` row first in the
     fold does not choose the field. The `body` rows are held and searchable, and only the
     wrong row is quarantined. Killed: the fill not skipping rows rejected for another reason.
   - `the_field_a_fold_judged_by_is_the_field_it_records` (2c, found in implementation): the
     row that chose `prose` is deleted in the same fold, and `prose` is still recorded, over an
     existing index and a new one. Killed: the judged field not recorded, on either path.
3. **A foreign fold indexes the writer's field.** `a_foreign_fold_indexes_the_writers_field`: a
   `body` engine that wrote nothing folds a `prose` row. The schema records `prose`, and a
   `prose` text query finds the row.
   - Red on the parent: an empty text field.
4. **The stamp is never served.** `the_text_stamp_is_never_served`: a scan after the write, the
   flush and the fold carries no `$text`. A vector-only row quarantined for its width exports
   no stamp.
   - Green on the parent by design.
   - Killed: `$text` left out of `stripped`, and every row stamped.
   - `a_patch_that_adds_text_fills_the_text_field` kills the fallback fill dropped.
5. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on M36.1, and on this ledger's commit.
   - The sweep over M36's source diff: 29 mutants over `5dbeb19..0c41cf0`, 15 caught, 8 unviable, 6 timed out (a write, a flush or a schema made to vanish leaves a test waiting), **0 missed**.
   - Hand mutations: 11, all killed. A twelfth, `implied`'s own fill removed, survived. It was
     redundant, because the reject pass fills every created schema, so the fill was removed.

**Residue (code review, minor; recorded in BACKLOG rows 55 and 56):**
- a rolling upgrade leaks the stamp into segments while a pre-M36 engine folds (the spec's
  risk);
- an unstamped row is still judged by the folding engine's field, as before, so whether a row
  with text under another attribute has text depends on which engine folds it;
- a scan's filter is applied before reserved names are stripped, so a filter on `$text` or
  `$metric` matches unfolded rows only;
- a field can be recorded from a row that is never sealed, consistently with the field that
  row was judged by (test 2c).
