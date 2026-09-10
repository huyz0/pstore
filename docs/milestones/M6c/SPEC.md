# M6c — The text field a segment does not name

**Serves:** the placeholder [M5c](../M5c/SPEC.md) left behind — *"`DEFAULT_TEXT_FIELD` is a
placeholder, exactly as `DEFAULT_FIELD` was for named vector fields, and it is replaced by the
same thing"* — and D-73 at field granularity, which `run.rs`'s existing text-field check
already cites.

**Depends on** [M3b](../M3b/SPEC.md), which solved this for vector fields and left both the
mechanism (an additive section) and the argument (`Section::Fields`'s own doc) in the tree.

## ⚠️ What this is, and what it deliberately is not

It is **not "a schema"**. M5c and M6a both said the replacement was "a schema in HEAD, which is
M6's catalog", and M6a's catalog carries index *names* and no field types. A schema is the
tenant-facing policy question — who may set the field, what happens to segments written under
the old setting, how it reaches a writer — and it needs the server that does not exist.

This is the **format and engine half**: a segment can be built over a named attribute, it
records which, and every reader is told rather than assuming. The name is set on the writer
handle, not by a tenant.

## ⚠️ The failure this milestone exists to prevent

`Engine::seal` calls `text::build(docs, DEFAULT_TEXT_FIELD)` — the literal `"text"`
([lib.rs:276](../../../crates/pstore-engine/src/lib.rs)) — and nothing in the segment says so.

1. A tenant whose prose is in `body` gets an **empty text index**, silently: `build` finds no
   `Value::Str` under `"text"`, writes no postings, and the corpus is indexed as far as anyone
   can tell. There is no way to fix it, because there is no way to say which attribute.
2. ⚠️ **A reader has no way to know what it is reading.** `run.rs:169` compares the requested
   field against the constant. That check is right — *"a request naming another would otherwise
   be answered with the `text` field's ranking and nothing anywhere would say so"* — and it is
   right only while every segment is built over `"text"`. Naming the field without moving that
   check re-introduces exactly the wrong answer it was written to stop, which is why the query
   change is in this milestone and not the next.
3. The day a segment is built over something else, every segment already written is ambiguous:
   its postings were built over *something*, and nothing records what.

## The numbers this milestone is pinned to

| Name | Value | Why this value |
|---|---|---|
| New section | **`TextFields = 14`** | The next free id (`Fieldnorms = 13` is the highest assigned). ⚠️ A **new** section, never a `Fields` row: `text.rs` already records why — a text field in that table is handed to `decode_field`, which reads postings as `f32` and returns a dense field of noise. |
| Where it lives | **the meta region**, beside `Fields` and the block index | ⚠️ A body section would need its own fetch on every cold open. `decode_fields` reads only bytes inside the suffix read, and `text_fields()` must answer at open time or the query check costs a round trip. |
| Written when | **a text index exists**, always — not only when the name differs | One path. A "write it only when non-default" rule leaves the common path untested and makes absence mean two things in newly written segments. |
| Absence | **means `"text"`** | Every pre-M6c segment has no such section and was built over `"text"`. This is how `Fields` treats its own absence, and it is what makes this landable with no migration. |
| Size | **~20 bytes per segment** | A count and one short name, once per segment — not per posting, which is the distinction M5d got wrong and was amended for. |

## Delta

**Format** (`pstore-format`)
- `Section::TextFields = 14`: a `u32` count then that many names.
- `SegmentWriter::with_text_fields(names)`, encoded into the meta region.
- `Segment::text_fields() -> &[String]` — the recorded names, or `["text"]` when the section is
  absent **and** the segment carries `TextPostings`, or empty when it carries neither.
- `SegmentWriter::without_text_fields_section_for_test()`, the sibling of
  `without_fields_section_for_test` and for the same reason: criterion 2 needs a pre-M6c
  segment, and one that cannot be built cannot be tested.
- ⚠️ The meta patch loop is **generalized** from its `patched ∈ {1, 2}` special case to a list.
  Its own comment says a wrong slot here "corrupts silently rather than fails"; a third
  hardcoded arm is the way that happens.

**Query** (`pstore-query`)
- The text field check moves from `Runnable::try_from` to `leg`, and compares against
  `segment.text_fields()` rather than the constant. `Runnable::Text` carries its field again.

**Engine** (`pstore-engine`)
- `Engine::with_text_field(name)`, defaulting to `"text"`. `seal` takes the field as an
  argument and records it.
- ⚠️ **`compact` takes the field from its input segments, never from the handle.** `seal` is
  reached by both `fold` and `compact`, and a compaction re-analyzes the original attribute
  (`text.rs`: postings cannot be inverted back into text). A handle left on the default
  compacting a `body` index would rebuild it over `"text"` and write **no postings at all** —
  a text index destroyed by a merge, with no error. Inputs that disagree are refused.

**Does not add** — a tenant-facing schema, any way to *change* the field on an existing index,
or a catalog field for it: those need the server. **More than one text field per segment** —
the table holds one, and the point is that a second no longer needs a format change; a second
would also need its own `Fieldnorms` section, which this does not build. **A refusal for a
document whose named attribute is not a string** — considered and rejected below.
**Analyzers** — still M5c's list.

## Acceptance criteria

1. A segment sealed over `body` reports `text_fields() == ["body"]`, and one sealed over the
   default reports `["text"]` **from the section**, not from the fallback.
2. ⚠️ A segment with **no** `TextFields` section reads as `["text"]` and its text query returns
   exactly what it returned before this milestone — the mixed-version corpus that
   `key-layout.md` calls the normal state. Constructed with
   `without_text_fields_section_for_test`, because forward compatibility that cannot be built
   cannot be tested.
3. A segment with no text index emits **no** `TextFields` section, and `text_fields()` is empty.
4. ⚠️ With three meta-region entries present, every other section still decodes: `Fields`, the
   block index and `TextFields` all resolve to their own bytes, and `INDEX_BUDGET` still holds.
5. A corpus whose prose is in `body`, sealed with `Engine::with_text_field("body")`, answers a
   `body` text query with the documents that contain the term — and the same corpus sealed
   without naming it answers nothing.
6. ⚠️ The query refuses a name the segment does not carry, **in both directions**: `body`
   against a default segment, and `text` against a `body` segment, both `UnknownField`. An
   empty result is not the failure mode; a *confident wrong ranking* is.
7. ⚠️ Compacting a `body` index through a handle configured with the **default** preserves the
   index: the merged segment still answers the `body` query, and still records `body`. Inputs
   naming different fields are refused rather than merged.
8. Region coverage ≥95% on the changed crates, mutation ≥80% on the changed modules, the full
   gate set green, and `scripts/ndcg.sh` and `scripts/depth.sh` above their floors.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `a_segment_records_the_text_field_it_indexed` | the name written and never read back; the count written as zero; the fallback shadowing a present section |
| 2 | `a_segment_without_the_section_reads_as_the_old_default` | absence read as "no text field", which refuses every query against every existing segment |
| 3 | `a_segment_without_text_writes_no_text_fields_section` | ~20 bytes on every vector-only segment, and `text_fields()` claiming a field that has no postings |
| 4 | `three_meta_entries_still_resolve_to_their_own_bytes` | the patch loop's `patched == 2 && n == 0` arm left in place — which returns `Fields` bytes as the block index, silently |
| 5 | `a_named_attribute_is_the_one_indexed` | `build` given the constant rather than the configured name — the milestone itself, and criteria 1–4 all pass without it |
| 6 | `a_query_naming_the_wrong_text_field_is_refused` | the check comparing against `DEFAULT_TEXT_FIELD` after the segment learned its own name, or dropped entirely — D-73's wrong answer, restored |
| 7 | `a_compaction_does_not_rebuild_over_the_wrong_field` and `a_compaction_of_disagreeing_inputs_is_refused` | `seal` reading the handle instead of the inputs — which passes every criterion above and silently empties a text index on the first merge |
| 8 | the existing format, index, query and engine suites | a section id colliding with `Fieldnorms`, which `Segment::open` resolves last-wins |

⚠️ Criteria 5, 6 and 7 are the milestone. 1–4 all pass over a writer that records the name
faithfully and then indexes and queries `"text"` anyway.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| Text query, accepted | unchanged | unchanged | unchanged — the table rides in the suffix read that already opens the segment | 0 |
| Text query, **refused** | 0 | **0 → 1** | 0 → the open round | 0 |
| Sealing a segment | unchanged | unchanged | unchanged | 0 |

⚠️ The second row is a real regression and it is deliberate. `run.rs` refuses before any I/O
today, and it can only do that because it knows the answer without the segment. Once the
segment carries the name, an error path costs the open round — which is the price of not
answering the wrong field confidently.

## Risks

- **The meta patch arithmetic.** Fixed-width entries and a hand-computed slot; a third meta
  section is exactly what its comment says corrupts silently. Criterion 4 is aimed at it, and
  generalizing the loop rather than adding an arm is why it is a task and not a line.
- **`INDEX_BUDGET`.** ~20 bytes against 8,150 is not close, but `try_finish` refuses an
  over-wide segment (C-10) and this is a third thing growing the region that cannot be shrunk.
- **The refusal moves after I/O**, undoing an explicit earlier decision. Its structural half —
  "refused once, because the type has no `Text` variant to fall through" — is weakened, so
  criterion 6 asserts the refusal rather than the empty result.
- **A tenant's index spans segments, and they may now disagree.** Criterion 6 refuses per
  segment, so a rolling change of field would make a fan-out fail against some segments and
  answer from others. Nothing changes a field today — that is the schema this defers — and the
  refusal is the safe direction, but it is the shape the schema milestone has to resolve.
- **Rejected: refusing a document whose named attribute is not a string.** It sounds like the
  same silent-loss family, and it is not the same shape. `check_storable` takes a `Document`
  and no field name, so it would need a signature change at both doors; and a refusal
  conditional on *other documents in the batch* would accept a document in one write and refuse
  the identical one in another. A batch-dependent door is a coin flip, not a check. Left as a
  named gap.

## Tasks

| Id | Commit |
|---|---|
| **M6c.1** | `Section::TextFields`, in the meta region, with the patch loop generalized |
| **M6c.2** | The query asks the segment which text field it carries |
| **M6c.3** | The engine indexes the attribute it is told, records it, and carries it through a compaction |

⚠️ In that order. M6c.3 before M6c.2 leaves a tree where a `body` segment answers a `text`
query with `body`'s ranking — the wrong answer this milestone exists to prevent, committed.
