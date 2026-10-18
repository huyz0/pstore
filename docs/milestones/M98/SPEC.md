# M98 — A named field's width is in the schema, and refused at the door

**Serves:** backlog row 65's open half. Since M61, nothing records a named field's width. So once
a field's rows are folded, or after a restart, the door accepts any width. Two widths folded apart
are then refused at query time, and their compaction fails (M61 SPEC, "The writer refuses a field
of mixed widths"). The row's other half, a dense search over a named field, is already delivered:
an exact scan in [M77](../M77/SPEC.md), split across servers in [M78](../M78/SPEC.md), and
clustered in [M83](../M83/SPEC.md).

## What is true today

These facts come from reading the tree at `cc714c2`. Nothing here was measured.

- **The schema** (`head::IndexSchema`) holds:
  - `dims` (the width of `vector`),
  - `text_field`, `metric`, `fts`, `trigram`.
  - **No width for any named field.**
- **The door** (`door_conflict`) checks a named field's width with `field_width_conflict`. That
  compares against the rows this process holds unfolded and the batch itself, **never HEAD**.
- **The flush's once-per-process check** (`batch_conflict`, the same as `row_conflict`) does not
  look at named fields.
- **The fold** quarantines a row whose named field disagrees with *this fold's* first row of that
  field (`odd_width` over a fold-local map). It does this twice: once in the reject pass, and again
  over the rows a patch resolves to.
- **The quarantine export** works each row's reason out again from `row_conflict` only, so a row
  refused over a named field's width shows `reason: null`.
- `crates/pstore-server/tests/multi.rs` `a_named_fields_width_is_one_an_index` **pins the gap**:
  - "With every row folded … a new width is accepted";
  - and the query that meets both widths is then refused.

## Delta

1. **Schema.** `IndexSchema` gains `fields: BTreeMap<String, u32>`.
   - It holds the **stored** width of each named dense field the index's rows have carried, which
     is the same convention as `dims`.
   - `vector` is never a key: its width is `dims`. Nor is a field derived from another (M67's
     `name$fde`), whose width its encoding's parameters give.
   - Inferred, and immutable afterwards, exactly as `dims` is.
2. **One check, in `row_conflict`.**
   - A row whose named dense field has a vector whose width differs from the schema's width for
     that field conflicts, with
     `SchemaConflict { what: "a named vector field's width", expected, got }`.
   - `expected` and `got` are in client widths.
   - Because the check lives in `row_conflict`, everything that already calls it gets it with no
     further code: the door with a cached schema, the flush's check, the fold's reject pass, `filled`,
     and the quarantine export's reason.
   - **A field the schema does not record** is still checked against the held rows and the batch
     (`field_width_conflict`), as in M61. Its map is seeded with the schema's widths (code
     review), so those rows only decide fields the schema does not record: the schema is
     authoritative (M35).
3. **Filled by the fold.**
   - The fold's width map is **seeded from the schema's `fields`**, in both of its passes. The
     second pass, over the rows a patch resolves to, is seeded from the first pass's result.
   - After both passes, the index's schema records every field in the map. Recording only adds
     (code review): a width already recorded never changes, and never goes.
   - The replica fingerprint includes the widths, and is unchanged when there are none (code
     review): a fold can record a width without changing a segment.
   - This applies to a new index's schema and to an existing schema alike, and it happens whether
     or not anything is left to seal, as the text field is (M36).
4. **HEAD.** A new trailing section, after externals, holds each schema's `fields`:
   - It covers live schemas and dropped ones, addressed by reference exactly as the full-text and
     trigram sections are: kind `0` + name, or kind `1` + position.
   - For each schema it holds a count, then `(name, u32 width)` pairs in name order.
   - It is written only when some schema has fields. When it is written, it forces externals, lens
     and dicts with a count of 0 (the M15.2 rule).
   - An entry that names no schema, or a field named `vector`, is `CorruptHead`.
   - A HEAD with no fields anywhere is byte for byte what the encoder at `cc714c2` wrote.
5. **API.** `GET /v1/indexes/{name}`'s `schema` gains `vectors: {name: client width}`.
   - The client width is the stored width less `metric.extra()`.
   - The key is omitted when the map is empty, so existing clients see no change.

**Not changed:** `pstore-format`, `pstore-query` and the writer.
- The query's per-segment width check stays. It is now the second line of defence, not the only
  one.
- Segments folded before M98 can still hold two widths. Nothing rewrites them, and HEAD records
  the width the first fold after M98 judged by (see Risks).

## Acceptance criteria

1. **Refused after a fold.** `a_named_fields_width_is_one_an_index` (server): once every row is
   folded, a write of `late` at another width is `400 schema_conflict`, and the index's rows are
   unchanged. The test's old "accepted" step becomes this refusal.
2. **Refused after a restart.** `a_named_fields_width_survives_a_restart` (server): a new `Api`
   over the same store, holding no rows, refuses the other width with `400 schema_conflict` and
   accepts the recorded one.
3. **Recorded in HEAD, and reported.**
   - `the_fold_records_each_named_fields_width` (engine): after one fold of rows carrying `late`
     at 3 under `euclidean_squared`, HEAD's schema has `fields == {late: 4}` (stored: 3 + 1).
   - A second fold carrying `late` at 3 and a new `other` at 2 adds `other` and leaves `late`.
   - A derived encoding (M67's `late$fde`) is never a key.
   - The server's `GET` reports `"vectors": {"late": 3}` in the restart test above (client width
     under `euclidean_squared`).
4. **The fold judges by the schema.** `a_held_width_meets_the_recorded_one_in_the_fold` (engine):
   a row of `late` at another width, made durable past the door with the test hooks, is quarantined
   by the next fold. Its export gives a reason containing `named vector field`, where M61's gave
   `null`. The fold commits, and a compaction afterwards succeeds.
5. **Each pass is seeded.**
   - `a_patch_meets_the_recorded_width` (engine): in an index folded with two widths before
     M98, a patch that resolves a row of the width the schema now contradicts is set aside, and
     its patch is not applied.
   - `the_schema_outranks_a_held_width_at_the_door` (engine): a row held from before the schema
     recorded `late` does not refuse a write at the recorded width.
   - `a_fold_without_a_field_keeps_its_width` (engine): a fold of rows without `late` leaves its
     width recorded.
   - `a_named_fields_width_is_part_of_the_fingerprint` (replica unit test).

   And `a_patch_beside_a_new_width_never_stops_the_fold` (engine, M61's)
   still passes, with its setup moved to the legacy state, which is the only one where that path is
   still reachable:
   - its fields are cleared with `commit_head_for_test`;
   - its new-width row is written by a second engine started on that HEAD, through its own door.

   Its assertions are unchanged.
6. **HEAD round-trips and stays compatible** (`crates/pstore-engine/tests/head.rs`):
   - `named_field_widths_round_trip`: live schemas and dropped ones round-trip.
   - A HEAD without fields encodes byte for byte as before.
   - A fields entry naming no schema, and a `vector` key, are `CorruptHead`.
   - The every-section truncation test grows from 14 to 15 decodable cuts.
7. **Gates.**
   - Each new or changed test is seen red first.
   - `./scripts/gates.sh` passes, and so do the coverage floors (95/95, `--all-features`).
   - `./scripts/mutants.sh` misses 0 on the changed lines, or each miss is argued equivalent.

## Test plan

| Test | Catches (mutation) |
|---|---|
| `a_named_fields_width_is_one_an_index` | the `row_conflict` field check deleted, or `!=` → `==` |
| `a_named_fields_width_survives_a_restart` | `fields` not encoded (the new process's cached schema is empty) |
| `the_fold_records_each_named_fields_width` | the recording loop deleted; an overwrite of a recorded width; client and stored width confused; the API key omitted |
| `a_held_width_meets_the_recorded_one_in_the_fold` | the fold's map left unseeded; the quarantine reason lost |
| `a_patch_beside_a_new_width_never_stops_the_fold` | pass two unseeded (fold-local again) |
| `a_patch_meets_the_recorded_width` | pass one unseeded, or pass two (checked by hand in review) |
| `the_schema_outranks_a_held_width_at_the_door` | the door's map seeded from held rows alone |
| `a_fold_without_a_field_keeps_its_width` | recording that assigns rather than adds |
| `a_named_fields_width_is_part_of_the_fingerprint` | the widths left out of the fingerprint |
| `named_field_widths_round_trip` and the truncation test | section order, a forced section's count, the `CorruptHead` arms |

## RA budget

**Zero new requests.**
- The door reads only the schema it already caches (M35).
- The fold writes the widths into the HEAD it commits anyway.
- HEAD grows by about `name + 4` bytes per named field, per index that has one.

## Risks

- **Legacy indexes.** If an index folded two widths before M98, the first fold after M98 records
  whichever width it meets first. Rows at the other width already in segments stay, and so does
  their query-time refusal, until the index is rewritten. Learning the width from the segments
  would cost a read per segment at the fold, which the cost rule forbids.
- **A reader from before M98** decodes HEAD, stops before this section, and re-encodes HEAD
  without it, as it does with dicts (M32). The widths are then lost, and the next fold after M98
  learns them again from its own rows. During a rolling upgrade that leaves M61's protection, not
  less.
- **Judged on what the fold read, not on what it seals** (code review), as the text field is
  (M36): a field first met in a row superseded, deleted or refused by its condition in the
  same fold still records its width.
- **Immutable.** A field whose rows are all deleted keeps its width, as `dims` does. Changing it
  means a new index, the same migration path `dims` has.

## Tasks

- **M98.1** Schema field, HEAD section, round-trip and truncation tests.
- **M98.2** `row_conflict` check, fold seeding and recording, engine tests.
- **M98.3** API `vectors`, server tests, the backlog row struck, ledger.
