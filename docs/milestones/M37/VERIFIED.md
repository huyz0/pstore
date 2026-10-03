# M37 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, on `MemoryStore`. Every result is a
test outcome.

Command: `cargo test -p pstore-engine --test schema`.

⚠️ **A test that fails to compile is not counted as red.** Each criterion names a red on the
parent, or the hand mutation that was seen to fail it.

1. **The default writer stamps nothing.** `the_default_field_is_never_stamped`: a default engine's
   texted row and its vector-only row, both quarantined for width, export no `$text`.
   - Red on the parent: `$text` = `text`.
   - Killed: the default writer stamping.
2. **An unstamped row is the default field's.** `an_unstamped_row_is_the_default_fields`: a row
   buffered past the door, as an older build's, is folded by a `body` engine. The schema records
   `text`, and a `text` query finds it.
   - Red on the parent: an empty field.
   - Killed: an absent stamp read as no field.
3. **A non-default writer's row is never read as the default's.**
   `a_custom_writers_row_is_never_the_defaults`: a `body` row with an ordinary `text` attribute,
   folded by a default engine, records no field. A later `body` row and a compaction succeed.
   - Red on the parent: `text` recorded.
   - Killed: the seal indexing the engine's own field (the compaction is refused), and a
     stamp read as text without text under it.
   - 3b: `a_sealed_custom_row_keeps_its_field_across_a_patch` (code review). A `color` patch,
     by id and by filter, after the fold leaves the field empty. Seen red with the base rows
     unstamped (`text` recorded).
4. **Every non-default row is stamped.** `the_text_stamp_is_never_served`, amended: a `prose`
   engine's vector-only row exports `$text` = `prose`, and no scan serves it.
   - Red on the parent: no stamp.
   - Killed: nothing stamped, and the condition inverted.
5. **A patch's text fills the field under its writer's field.**
   `a_patch_that_adds_text_fills_the_text_field` passes unchanged.
   - Killed: a patch unstamped, and `merged` keeping the base's stamp.
   - 5c: `a_patch_by_filter_that_adds_text_fills_the_text_field` was added when a by-filter
     patch left unstamped survived. Killed: that mutant.
   - 5b: `a_default_patch_keeps_a_custom_rows_stamp`. Red on the parent, and killed: `merged`
     taking a patch's stamp state when the patch sets no text.
6. **A patch that changes nothing touches nothing.** `a_custom_writers_no_op_patch_touches_nothing`:
   HEAD's segment references for the index are unchanged.
   - Green on the parent, by design.
   - Killed: the stamp compared in the no-op check.
7. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on M37.1, and on this ledger's commit.
   - The sweep over M37's source diff: 29 mutants over `650639a..06f1b8e`, 23 caught, 5 unviable, 1 timed out (a write made to do nothing leaves a test waiting), **0 missed**.
   - Hand mutations: 12, all killed.

**Residue (code review, minor; recorded in BACKLOG row 56):**
- with a schema whose text field is empty, the fresh view still indexes this engine's own
  field until a fold. It holds only this engine's rows, but a default engine's fresh view can
  match a `body` writer's ordinary `text` attribute until then;
- a stamp-only change by a patch is skipped as a no-op, and the sealed row would be identical
  anyway;
- a user predicate naming `$text` sees the stamp on rows being resolved.
