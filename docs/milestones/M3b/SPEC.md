# M3b — Named and multi-valued vectors

**Serves:** D-28 (multi-vector is a document-level concept from the schema up), D-72
(generic impact payload), and `06-indexing/modalities-and-sequencing.md` §3.

⚠️ **D-73 is deliberately NOT served.** It asks for the `prefetch[] + fusion` request shape
in v1, and this milestone adds no query API — there is no layer 4 yet. Citing a decision a
milestone does not deliver is what makes an evidence ledger hollow, so it is named here as
owed rather than listed above.

**Why now, out of roadmap order.** `modalities-and-sequencing.md` §3 names three properties
the v1 data model must have "regardless of what is implemented". M3 shipped violating two:

| Requirement | M3 |
|---|---|
| Vectors **named and plural** — "a singular `vector` field is the migration trap" | ❌ `Document { vector: Vec<f32> }` |
| A document may own **many** vectors — retrofitting is "a rewrite" (D-28) | ❌ fixed width, one row one vector |
| Sections optional and self-describing | ✅ M3.4 |

Nothing is broken today because only dense is implemented. It breaks at M5a, and the
section mechanism from M3.4 is the expensive half and already exists.

## Delta

**Adds**
- `Document.vectors: BTreeMap<String, VectorField>`, where

  ```rust
  enum VectorField { Dense(Vec<Vec<f32>>), Sparse(Vec<(u32, Impact)>) }
  ```

  ⚠️ **Kind-tagged, not a bare vector list.** A sparse vector is `(dimension, impact)` pairs
  (D-72); representing one as a dense `Vec<f32>` over a 30k-term vocabulary costs **150×**
  the bytes — 12 TB against 0.08 TB at 100M documents. A model that can only hold dense
  arrays does not close the trap, it moves it to the same wall.
  `Sparse` is **accepted by the type and refused by the writer** with a named error: the
  shape must exist in v1, the retriever is M5a.
- `Impact` as an **opaque** type — a private field behind `new`/`get`. ⚠️ D-72 makes the
  impact *encoding* the configurable part (u8/f16/varint), so a public `f32` payload is
  precisely the retrofit this milestone exists to prevent. It cannot be carried to M5a,
  because carrying it is the thing that cannot be carried.
- A **`Fields` section** (new reserved id) describing each vector section: the directory
  entry it belongs to, the field name, kind, metric, dims, and `per_row` (0 = variable,
  with a row-offset table).
  ⚠️ **Additive, so there is no version bump.** Directory entries stay fixed-width and a
  segment without a `Fields` section reads exactly as it does today — one unnamed dense
  field. `modalities-and-sequencing.md` §3 promises "no version bump, no migration — old
  segments stay valid forever"; putting the names in the entries themselves would break
  that, and would also silently corrupt `writer.rs`'s fixed-width patch of the `Blocks`
  entry offset.
- **New reserved ids for the second and later fields.** ⚠️ Field 0 keeps the legacy
  `Vectors`/`RaBitQ`/`Sq8` ids; fields 1..*n* use `FieldVectors`/`FieldRaBitQ`/`FieldSq8`.
  Letting several sections *share* an id looks additive and is not: `Segment::open` does
  `sections.insert(id, span)`, last-wins, so a deployed M3-vintage reader meeting a
  two-field segment would find one arbitrary `Vectors` span and return **field b's vectors
  as the segment's vectors** — a wrong answer, not a skip, in the one direction the
  forward-compatibility mechanism exists to protect. Under this layout an old reader sees
  exactly the legacy single-dense view: correct, if partial.

**Does not add**
- **Sparse or multi-vector *search*.** Storage and addressing only. Scoring is M5a/M6.
- **Per-field clustered search.** `VecIndex` holds one centroid object and one row order; a
  second dense field needs its own clustering. Criteria 4 and 7 are therefore asserted at
  the **section-fetch** level, which is where field isolation actually lives, and the
  clustered multi-field case is carried to M5a **explicitly** rather than implied.
- A schema registry. The segment is self-describing, which is what a reader needs.

## Acceptance criteria

1. A document round-trips through a segment with **several named fields**, each recovering
   its own vectors byte-identically, and names survive — not positions.
2. A field holding **many vectors per document** round-trips with the same count and order,
   and its row boundaries are recovered from the offset table.
3. Each field's `Vectors`/`RaBitQ`/`Sq8` sections occupy **non-overlapping** ranges and
   every one is described by a `Fields` row.
4. **Reading field *a*'s sections moves zero bytes of field *b*'s**, asserted with the
   per-section byte counter at `coalesce_gap = 256` and sections ≥ 4 KiB apart. ⚠️ The gap
   is named because the default 64 KiB merges adjacent sections and would make this pass by
   measuring the coalescer — the mistake M3's zone-map and exact-rerank tests both made.
5. A field the segment does not carry is an **error**, not an empty result.
6. **Both directions of forward compatibility**, because only one of them was tested and the
   untested one fails silently:
   * a segment written *before* `Fields` existed still opens and reads as one dense field;
   * a reader that knows nothing about `Fields` sees **exactly field 0** in a multi-field
     segment — not an arbitrary field, and not a mixture. ⚠️ This is the direction a shared
     section id breaks, and it breaks by returning a wrong answer rather than an error.
7. A segment with **8 fields** keeps its meta region within `INDEX_BUDGET`, so a cold open
   stays one round; beyond what fits, the writer **refuses rather than silently spending a
   second round trip**. ⚠️ M3's budget said "3 depth" with nothing bounding the field count.
8. **A sparse field is refused by the writer** with an error naming M5a — not stored wrongly,
   not silently dropped.
9. **`Impact` exposes no representation.** Its field is private and its API is `new`/`get`,
   so M5a can change the storage to u8, f16 or varint without touching `Document`. Enforced
   by the compiler — rung 1 of the gate ladder — and demonstrated by a round-trip test.
10. A fixed-width field carries **no row-offset table**: `per_row != 0` and the section is
   exactly `rows × dims × 4` bytes. ⚠️ Asserted directly, because a needless table costs 4
   bytes a row and changes neither recall nor query bytes, so no existing gate sees it.
11. **`scripts/recall.sh` is unchanged** at its floor, and a single-field cold query is still
    **≤3 rounds from `HEAD`**.
12. Region coverage ≥95% on shipped crates, mutation ≥80%, full gate set green.

## Test plan

| # | Test that must fail first | Mutation it catches |
|---|---|---|
| 1 | `a_document_round_trips_several_named_fields` | a "named" map that collapses to one field |
| 1 | `field_names_survive_a_new_field_being_added` | names encoded as positions, so adding one renumbers the rest |
| 2 | `a_field_may_hold_many_vectors_per_document` | a layout that keeps the first and drops the rest |
| 2 | `a_variable_length_field_recovers_its_row_boundaries` | an offset table off by one row |
| 3 | `every_vector_section_has_a_fields_row` | a section written with no description, unreadable by anything but its writer |
| 4 | `reading_one_field_moves_no_bytes_of_another` | fields interleaved per row rather than sectioned |
| 5 | `an_absent_field_is_an_error_not_an_empty_answer` | a miss returning zero hits, indistinguishable from "no matches" |
| 6 | `a_segment_without_a_fields_section_still_reads` | a reader that requires the new section, breaking every existing segment |
| 6 | `a_fields_blind_reader_sees_field_zero_not_an_arbitrary_one` | several fields sharing a section id, where last-wins returns another field's vectors as the segment's |
| 7 | `eight_fields_still_open_in_one_round` | a directory that grows past the suffix read unnoticed |
| 7 | `a_segment_too_wide_to_open_in_one_round_is_refused` | silently spending a second round instead |
| 8 | `a_sparse_field_is_refused_with_a_named_error` | sparse stored as dense, which "works" and costs 150× |
| 9 | `an_impact_round_trips_through_its_api_not_its_field` | a public `f32` payload, which makes the encoding a breaking change in M5a |
| 10 | `a_fixed_width_field_has_no_offset_table` | a table on every field, invisible to recall and byte gates |
| 11 | `a_cold_query_from_head_costs_three_round_trips` (M3) | a per-field fetch loop |

## RA budget

| Operation | Budget |
|---|---|
| Cold query, one field | **3 depth** (unchanged) |
| Reading *f* fields' sections | **1 round**, *f*× the bytes — all spans known at open |
| Cold open, ≤8 fields | **1 round**; beyond that the writer refuses |
| Build | 1 W per segment, unchanged |

## Risks

- **The `Fields` section competes with the block index for the suffix read.** Both live in
  the meta region under one `INDEX_BUDGET`. Criterion 7 bounds it and makes the writer
  refuse rather than quietly cost a round trip; without that, adding fields degrades every
  query's depth with nothing reporting it.
- ⚠️ **An earlier draft of this spec stated the `Impact` constraint and violated it in the
  same sentence** — "`f32` here… but the type must not assume `f32` either". It is now
  opaque (criterion 9). The lesson is worth keeping: a risk that describes the defect it is
  shipping is not a mitigation.
- **Per-field clustered search is deferred, and criteria 4/7 are narrowed accordingly.**
  Stated rather than implied: the clustered multi-field path is unimplemented, not
  unmeasured.

## Tasks

| ID | Task |
|---|---|
| M3b.1 | `Document.vectors` and `VectorField`; single-dense constructor kept for callers |
| M3b.2 | The `Fields` section and the new field ids: additive both directions, no version bump |
| M3b.3 | Per-field `Vectors`/`RaBitQ`/`Sq8` sections, fixed and variable width |
| M3b.4 | Field-aware fetch in `Segment::scan`/`search` and `VecIndex`; absent field errors |
| M3b.5 | Bounds and isolation: 8-field open, writer refusal, byte isolation, no stray tables |
