# M3b — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md), naming the test or command that
demonstrated it.

Gate: `scripts/check-verified.py`.

1. `a_document_round_trips_several_named_fields`, `field_names_survive_a_new_field_being_added`
   (`cargo test -p pstore-format --test fields`),
   `a_document_carries_several_named_fields` (`--test vector_fields`).
   Names, not positions: a field that sorts first does not renumber the others.
2. `a_field_may_hold_many_vectors_per_document`,
   `a_truncated_variable_width_section_is_an_error`.
   `cargo test -p pstore-format --test fields`. Variable-width fields carry a row-offset
   table of `rows + 1` entries, so the last row is not a special case.
3. `every_vector_section_has_a_fields_row`, `a_fields_code_sections_are_addressable_by_name`,
   `named_sections_do_not_overlap` (`--test sections`).
   All three of a field's sections — vectors, 1-bit, int8 — are named, not just its vectors.
4. `reading_one_field_moves_no_bytes_of_another`.
   `cargo test -p pstore-format --test fields`. At `coalesce_gap = 256`: the default 64 KiB
   merges adjacent sections and would make this pass by measuring the coalescer, which is
   the mistake M3's zone-map and exact-rerank tests both made.
5. `an_absent_field_is_an_error_not_an_empty_answer` (`--test fields`),
   `a_query_names_the_field_it_searches` (`cargo test -p pstore-index --test persisted`).
6. **Both directions of forward compatibility**, which is the criterion the spec review
   added after finding only one was tested:
   * `a_segment_without_a_fields_section_still_reads` — a segment written before the
     `Fields` section existed opens as one dense field, **with no version bump**. Putting
     names in the directory entries would have needed one, breaking every existing segment
     against `modalities-and-sequencing.md` §3.
   * `a_fields_blind_reader_sees_field_zero_not_an_arbitrary_one` — ⚠️ the direction that
     fails *silently*. `Segment::open` keys sections by id, last wins, so several fields
     sharing `Vectors` would make a reader predating the table return **another field's
     vectors as the segment's**. Fields after the first therefore use their own ids.
7. `eight_fields_still_open_in_one_round`, `a_segment_too_wide_to_open_in_one_round_is_refused`
   (`--test fields`), `several_fields_are_read_in_one_round`.
   ⚠️ M3's budget said "3 depth" with **nothing bounding the field count**. The `Fields`
   table shares `INDEX_BUDGET` with the block index, and the writer's fitting loop can only
   shrink the latter — so past some width a cold open silently costs a second round trip and
   every query a fourth. The writer now refuses instead. Verified sensitive: a fetch loop
   takes three fields to three round trips.
8. `a_writer_refuses_only_what_it_still_cannot_store` (`--test vector_fields`),
   `a_write_the_format_cannot_store_is_refused_at_the_door`
   (`cargo test -p pstore-engine --test engine`).
   ⚠️ **This criterion was written because M3b.1 shipped silent data loss.** Making the model
   expressible ahead of the format meant a document with a field not named `vector`
   round-tripped to *nothing* — `d0 fields=[]` — because the writer read one field by name
   and any other yielded an empty slice. Refusal at the door turned that into a loud
   failure; M3b.3 then made those documents storable and the refusal narrowed to sparse.
9. `an_impact_round_trips_through_its_api_not_its_field` (`--test vector_fields`).
   `Impact`'s field is **private**, so D-72's configurable encoding (u8/f16/varint) can be
   chosen in M5a without touching `Document`. Enforced by the compiler — rung 1 of the gate
   ladder — rather than by the test, which only pins the API.
10. `a_fixed_width_field_has_no_offset_table`. Asserted as an exact section size, because a
    needless per-row table changes neither recall nor query bytes and no other gate sees it.
11. `./scripts/recall.sh` → **0.9810 unchanged** on 20,000 × 384d clustered synthetic, and
    `a_cold_query_from_head_costs_three_round_trips` still passes.
12. `./scripts/coverage.sh --fail-under-regions 95` → **95.26% region**, 97.19% line on
    shipped crates, exit 0. `cargo mutants --workspace` → 1,605 mutants, 1,246 caught, 170
    missed, 2 timeouts: **87.9% workspace, 89.0% on shipped crates** (up from M3's 83.0% /
    84.4%). `./scripts/gates.sh` → green.

    ⚠️ **170 mutants survive and are not claimed to be covered.** The largest groups are
    unchanged from M3 and are structural rather than neglected: `object_store_backend.rs`
    (25) needs a live cloud backend and is M0a.13; `rabitq.rs` (22) is arithmetic inside an
    estimator whose output is consumed by a *ranking*, so a small perturbation reorders
    nothing and no test can see it; `sim.rs` and `faulty.rs` are PRNG mixing constants,
    where a mutated mixer is still a mixer and the property under test is determinism.
    `lire.rs` (18) is the OQ-51 spike, whose verdict is a measurement rather than an
    assertion.

## Bugs this milestone found in itself

Recorded because each was invisible to every gate that existed when it shipped.

- **Silent data loss** between M3b.1's model and M3b.3's layout (criterion 8 above).
- **A ragged dimension corrupted the centroid table.** A document of a different width
  reached the *clustering*, where farthest-point seeding picked the outlier as a centroid;
  `Centroids::encode` writes one `dim` for all of them, so the spans decoded to a request for
  bytes `16477954617..457680856537` of a 218 KB object. The field's width is now defined once
  and both the clustering and the layout use it. The branch was labelled "unreachable".
- **A corrupt offset table panicked** rather than erroring — `table + lo` on values read from
  the segment. A decoder must never panic on malformed input.
- **Maintenance could leave empty posting lists**, which cost a centroid, a directory entry
  and a probe that can never return anything, accumulating over exactly the cycles LIRE
  exists to allow.
- **`sq8::Code::codes()` was dead** once the wire form replaced it, and was deleted rather
  than covered.

## What this milestone does not do

- **Sparse or multi-vector search.** Storage and addressing only; scoring is M5a/M6. A
  sparse field is representable and refused by the writer, which is what
  `modalities-and-sequencing.md` §3 asks for: "the shape must exist in v1 even if only
  `dense` is implemented".
- **Per-field clustered search.** `VecIndex` holds one centroid object and one row order, so
  a second dense field would need its own clustering. Criteria 4 and 7 are asserted at the
  section-fetch level and the clustered multi-field case is **carried to M5a**, stated rather
  than implied.
- **D-73's `prefetch[] + fusion` request shape.** It needs a query layer, which does not
  exist; named in the spec as owed rather than cited as served.
