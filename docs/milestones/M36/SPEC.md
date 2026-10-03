# M36 — A row carries its writer's text field, and every fold judges it by that

**Serves:** [BACKLOG](../BACKLOG.md) row 54, which [M35](../M35/VERIFIED.md) opened, and a wider
hole spec review measured while reviewing this milestone's first draft.

## What is true today

- **A row does not record which text field its writer was configured with.** Every check
  that judges a row's text reads the **folding** engine's `text_field`:
  - the reject pass (`row_conflict`);
  - the schema a fold creates for a new index (`implied`);
  - the fill of an empty text field (M30).
- **Measured in spec review, on the parent.** Engine A, configured over `prose`, writes and
  flushes `p`. Engine B (`body`) writes and flushes `a`, then folds. The schema records
  `body`; the index holds `a` **and `p`**; the quarantine is empty. `p` was sealed with its
  text unindexed, with no refusal and no count. The same happens when the index already exists
  with an empty text field.
- **Row 54, measured in M35 code review.** A row M35's waiver let through would be sealed the
  same way by a foreign fold. So M35 keeps text-field conflicts out of its waiver
  (`quarantinable`), and such a row still blocks its writer's whole lane until a restart.
- **The first draft of this spec** stamped only rows the waiver let through. Spec review
  rejected it (MAJOR 1, MAJOR 2):
  - it left the sibling race open;
  - `implied` and the M30 fill still ignored the stamp;
  - the stamp was never stripped.

## Delta

**The writer's text field travels on the row, as the metric does (M9d).**

1. **The door stamps every row that carries text.** A row carrying a non-empty string under the
   writer's `text_field` gets the reserved attribute `$text`: that field's name. This happens
   in the same loop that writes `$metric`, `$fts` and `$trgm`. A row without text is not
   stamped. A client cannot write `$text`, because every `$` name is already refused at the
   door and in a patch.
2. **One reader, `text_of(row)`, gives the field a row's text is under:**
   - `$text`, when the row carries non-empty text under that name (spec review round 2,
     minor 2: `merged` keeps a stamp after a patch unsets the text);
   - else this engine's field, when the row carries text under it. That covers a row from an
     older build, or one buffered by the test hook that skips the door;
   - else none.
   - Every check that judged by the folding engine's field reads this instead:
     - `row_conflict`, so the flush and the reject pass;
     - `implied`, for a new index;
     - the reject pass's schema, for an index whose text field is empty;
     - the fill itself (M30), and the sealing loop, which records the field the pass judged
       against rather than recomputing it.
   - ⚠️ **The field an empty schema takes is the first `text_of` of a row that passes every
     other check** (spec review round 2, major 1). A wrong-width row first in the fold must
     not choose the field and get every correct row quarantined: "the blast radius of a
     contradiction is the contradicting row".
   - So a fold's rows with any other field are quarantined, whichever engine runs it.
3. **The seal indexes the schema's field**, as it does today, now filled from the rows. So a
   `prose` row folded by a `body` engine into a `prose` index is searchable over `prose`.
4. **`stripped` removes `$text`**, so no segment, fresh view or scan ever carries it. The
   quarantine export shows it under `reserved`, as it shows `$metric`.
5. **M35's waiver covers every conflict**, and `quarantinable` is removed.

**Not changed:**
- the door's checks;
- the waiver's lifetime (M35);
- a rowless patch: `merged` never copies a `$` name, so a patch that adds text is judged by
  the folding engine's field, as today (spec review, minor 7);
- `with_text_field` accepting a `$` name (spec review, nit 8).

## Acceptance criteria

1. **Any fold quarantines a waived text-field conflict.** M35 code review's scenario:
   - an engine over `prose` writes a texted row before the index has a schema;
   - an engine over `body` folds the index;
   - the first engine's flush refuses, and its next flush writes the row;
   - a fold by the `body` engine leaves only `a` in the index, the row in the quarantine with
     `$text` = `prose` under `reserved`, and `schema_rejects` = 1.
   - **Parent:** the second flush is refused.
2. **The sibling race is caught** (tests 2 and 2b). Engines over `prose` and `body` each write and flush one
   texted row to a new index, and one of them folds. The index holds exactly one of the two,
   the other is in the quarantine, and the held row is found by a text query over the
   recorded field. Then the same over an index that already exists with an empty text field.
   - **Parent:** both rows held, the quarantine empty.
   - And a wrong-width `prose` row placed first does not choose the field: the `body` rows
     are held and searchable, and only the wrong row is quarantined.
3. **A foreign fold indexes the writer's field.** An engine over `prose` writes and flushes a
   texted row to a new index. An engine over `body`, which wrote nothing, folds it. The
   schema records `prose`, and a text query over `prose` finds the row.
   - **Parent:** an empty text field, and the query finds nothing or is refused, whichever the
     test sees first run on the parent.
4. **The stamp is never served.** After a write, after a flush and after a fold, a scan carries
   no `$text`. A texted row's stamp shows in the quarantine export (criterion 1). A vector-only
   row is not stamped: one quarantined for its width exports no `$text` under `reserved`
   (spec review round 3: an inert stamp would otherwise survive every test).
5. **Gates.** `./scripts/gates.sh` is green, and the sweep over M36's source diff misses 0.

## Test plan

In `crates/pstore-engine/tests/schema.rs`.

| # | Test | Red on the parent because / mutation it catches |
|---|---|---|
| 1 | `a_waiver_never_covers_a_text_field` → **renamed and inverted** `any_fold_quarantines_a_text_field_conflict` | the second flush refused; `row_conflict` not reading the stamp |
| 2 | `two_writers_text_fields_never_share_an_index` | both rows held; the reject pass's empty field not filled from the rows |
| 3 | `a_foreign_fold_indexes_the_writers_field` | the field empty; `implied` not reading the stamp |
| 2b | `a_wrong_row_never_chooses_the_text_field` | the first `text_of` taken from a row rejected for its width |
| 4 | `the_text_stamp_is_never_served` | ⚠️ a scan carries nothing on the parent, so this test is green there by design. It guards `$text` left out of `stripped`, and a stamp on a vector-only row |

⚠️ **M35's test is inverted on purpose, which is the fix landing.** It pinned the refusal as the
safe residue, and row 54 says so.

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Any | 0 | 0 | 0 | 0 | unchanged |

**Bytes:** each row carrying text grows by its field's name plus about 8 bytes of attribute
framing, in bundles only. A segment never stores the stamp.

## Risks

- ⚠️ **A rolling upgrade leaks the stamp** (spec review round 2, minor 3). An engine older than
  M36 ignores `$text` and does not strip it. While one folds, **every** texted row an M36
  engine wrote is sealed with `$text` as a visible attribute, and judged by the folder's own
  field, as today.
  - The symptom: a document read back carries `$text`, and re-writing it as read is refused,
    because `$` names are reserved.
  - Nothing removes it afterwards: a compaction carries a segment's attributes through.
  - A fleet should therefore upgrade every folder before any writer stamps. The project has no
    deployed fleet, so a two-phase knob (read and strip first, stamp second) is a new BACKLOG
    row, not this milestone's work.
  - Rows written by an older build carry no stamp, and a newer fold judges them by its own
    field, as today.
- **The export shows the stamp** for every quarantined row that carries text, whatever its
  reason (spec review round 2, nit 5). A client re-ingesting an export drops `reserved`, as it
  already must for `$metric`.
- **Two fields in one fold of a new index:** the first row's field wins. Rows are in lane
  order, so which writer wins is decided by lane number, not by time. Either way, the other
  rows are quarantined and counted, never sealed unindexed.
- **M17 retries** are unaffected (spec review, nit 6):
  - the stamp is on the memtable row, so every attempt encodes it;
  - each attempt's bytes are kept as they were PUT.

## Tasks

- **M36.1** — The stamp, `text_of`, its four readers, `stripped`, and the waiver; tests 1–4.
- **M36.2** — The ledger, `BACKLOG.md` row 54 closed, and the roadmap row.
