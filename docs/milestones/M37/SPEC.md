# M37 — An unstamped row was written under the default text field

**Serves:** [BACKLOG](../BACKLOG.md) row 55, and the first item of row 56. Both were opened by
[M36](../M36/VERIFIED.md).

## What is true today

- **M36 stamps `$text`**, the writer's text field, on every row that carries text under that
  field. `text_of` reads the stamp. With no stamp, it uses the **folding** engine's field.
- **The server never configures a text field.** Every engine it builds uses the default,
  `text` (`pstore_format::text::DEFAULT_TEXT_FIELD`). So in the only deployable binary, the
  stamp is always `text` and tells no reader anything.
- **That stamp is what leaks in a rolling upgrade** (row 55). An engine older than M36 does not
  strip it, so while one folds, every texted row is sealed with a visible `$text`
  attribute. A client that re-writes such a document as it was read is refused, because
  `$` names are reserved.
- **An unstamped row is judged by whichever engine folds it** (row 56, first item; M36 code
  review). Such a row comes from an older build, or from the test hook that skips the door.
  So whether its text is indexed depends on which engine folds it.

## Delta

**An absent `$text` means the default field, as an absent `$metric` means `dot_product` (M9d).**

1. **A writer whose field is the default stamps nothing.** The server's rows carry no stamp, so
   its bundles are byte-identical to before M36, and no build can leak what is not there.
2. **A writer whose field is not the default stamps every row it writes**, not only those with
   text.
   - An unstamped row now means "written under the default field". So a `body` engine's row
     carrying an ordinary attribute named `text` must say it was not written under `text`.
   - **Its patches too** (spec review, B1), by id and by filter. A delete carries no attribute
     and is never stamped.
   - **A merged row's writer is the patch's when the patch sets text under its own field**
     (spec review, B1 and round 2 m1). Then `merged` takes the patch's `$text`, or its
     absence; otherwise it keeps the base's.
     - So text a `body` engine's patch adds to a segment row is read under `body`, and the
       fold fills the field with it. Without this, every merged row was unstamped, so it read
       as the default field's, and `a_patch_that_adds_text_fills_the_text_field` went red.
     - A default engine's patch of `color` onto a `body` writer's row keeps the `body` stamp,
       so that row's ordinary `text` attribute is never read as default text.
   - **A stamp is never a change** (spec review, round 2 M1). M13.1's rule, that a patch which
     changes nothing touches nothing, compares a merge with its base **without** `$text`:
     - a segment row is stripped, so it never has a stamp;
     - a `body` engine's patch setting an attribute to its current value would otherwise
       always differ from its base, and cost a delete vector and a segment row.
3. **`text_of(row)`** is the row's writer field (its stamp, else the default) when the row
   carries non-empty text under that field, else none. It no longer reads the folding
   engine's field at all.
4. **The fold's seal indexes the recorded field only** (spec review, M1), and no text when it
   is empty. Before this change, it fell back to the folding engine's own field. Under M36 that fallback was only
   reached when `text_of` would have filled the field with the same name; under M37 it is not.
   For example, a default engine folding a `body` writer's row with an ordinary `text`
   attribute would build a `text` index the schema does not name. A later `body` fold would
   then leave the index uncompactable, the state M30 shut. The fresh view keeps its
   fallback: it indexes only this engine's own rows, whose writer field is this engine's.
5. Nothing else changes from M36: the reject pass, the fill, the recorded field, `stripped`, and
   the waiver.
6. `with_text_field("")` is a non-default field: it stamps `""`, which never carries text,
   because the door refuses an attribute named `""` (spec review, n1). `with_text_field("text")`
   stamps nothing: the comparison is by name.

## Acceptance criteria

1. **The default writer stamps nothing.** A default engine's texted row, and its vector-only
   row, are each quarantined for their width. Neither export carries `$text` under
   `reserved`.
   - **Parent:** the texted row's export carries `$text` = `text`.
2. **An unstamped row is the default field's, whichever engine folds it.** A row carrying text
   under `text` is buffered past the door, as an older build's would be, and flushed. An
   engine over `body` folds it into a new index. The schema records `text`, and a `text`
   query finds the row.
   - **Parent:** the field is empty, and the row's text is unindexed.
3. **A non-default writer's row is never read as the default's.** An engine over `body` writes
   a row with no `body` attribute and an ordinary `text` attribute. A default engine folds it
   into a new index. The schema records no text field, and the row is held. Then a `body`
   engine folds a `body` row into it, and a compaction of the index succeeds: no segment
   indexed a field the schema does not name.
   - **Parent:** the schema records `text`.
4. **Every non-default row is stamped.** A `prose` engine's vector-only row, quarantined for
   its width, exports `$text` = `prose`.
   - **Parent:** no stamp.
   - M36's `the_text_stamp_is_never_served` is amended to match. Its vector-only half asserted
     no stamp on a `prose` engine's row. That half now covers the default engine (criterion
     1), and the `prose` row is asserted stamped.
5. **A patch's text fills the field under its writer's field.**
   `a_patch_that_adds_text_fills_the_text_field` passes unchanged. It was red under this
   spec's first draft (spec review, B1).
5b. **A default writer's patch keeps a custom row's stamp.** A `body` engine writes criterion 3's
   row, a default engine patches its `color`, and a default engine folds the index. The schema
   records no text field.
   - **Mutation:** `merged` taking the patch's stamp state when the patch sets no text under
     its own field.
6. **A patch that changes nothing touches nothing, from any writer.** A `body` engine patches a
   folded row's attribute to its current value, and folds. HEAD's segments for the index are
   unchanged, and no delete vector is written.
   - **Mutation:** the stamp compared in the equality check.
7. **Gates.** `./scripts/gates.sh` is green, and the sweep over M37's source diff misses 0.

## Test plan

In `crates/pstore-engine/tests/schema.rs`.

| # | Test | Red on the parent because / mutation it catches |
|---|---|---|
| 1 | `the_default_field_is_never_stamped` | a stamp of `text`; the default writer stamping |
| 2 | `an_unstamped_row_is_the_default_fields` | the folding engine's field read for an unstamped row |
| 3 | `a_custom_writers_row_is_never_the_defaults` | a non-default writer not stamping a row without text |
| 4 | `the_text_stamp_is_never_served`, amended | a non-default writer stamping only rows with text. ⚠️ Its header comment changes too (spec review, m2): it is no longer green on the parent |
| — | 1 and 4 together | the stamp condition inverted (spec review, m3) |
| 5 | `a_patch_that_adds_text_fills_the_text_field` (existing) | a patch not stamped; `merged` keeping the base's stamp when the patch sets text |
| 5b | `a_default_patch_keeps_a_custom_rows_stamp` (⚠️ green on the parent, whose `merged` ignores every `$` name: a mutation guard, seen red against the named mutant) | `merged` taking a patch's stamp state when the patch sets no text under its field |
| 6 | `a_custom_writers_no_op_patch_touches_nothing` (⚠️ green on the parent, as 5b) | the stamp compared in the no-op check |

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Any | 0 | 0 | 0 | 0 | unchanged |

**Bytes:**
- the default writer: back to before M36;
- a non-default writer: its field's name, plus about 8 bytes, on every row and patch, in the
  memtable and bundles; never in a segment.

## Risks

- **A library engine older than M36, configured over another field**, writes unstamped rows,
  which an M37 fold now reads as the default field's: it quarantines them as conflicts
  against a non-default index, where M36 would have judged them by the folder's own field.
  Counted and exportable, never sealed unindexed. The server always uses the default, so
  it is not affected.
- **A rolling upgrade from M36**, for a library engine over another field: M36's rows carry a
  stamp whenever they carry text, and that reads the same under M37. ⚠️ Except a row with
  no text under its writer's field and an ordinary `text` attribute (spec review, m1). M36
  did not stamp it, so an M37 fold reads it as the default field's. That is criterion 3's
  case, left unprotected for bundles M36 wrote. It affects library engines only.

## Tasks

- **M37.1** — The stamp rule, `text_of`, tests 1–4.
- **M37.2** — The ledger, `BACKLOG.md` row 55 closed and row 56 narrowed, and the roadmap row.
