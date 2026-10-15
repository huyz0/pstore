# M91 — Parquet `list<float>` vector columns (PLANNED, not built)

**Serves:** backlog row 74's last item ("nested columns"). **Status: planned.** No owner decision
is needed; the scope is stated narrowly on purpose.

## What "nested" means here

The vector columns real pipelines write as Parquet are `list<float>` (the repo's own
`list.parquet` fixture), not `FIXED_LEN_BYTE_ARRAY`: a repeated `float` leaf with a repetition and
a definition level a value. Every row's list has the same length `D`, so the value bytes of a
page are `rows × D × 4` contiguous floats behind the levels. That is the same shape M84 reads
(levels, then a plain run), with **two** level streams instead of one.

## Decision

Read only the case the shape allows a computed range for, and refuse the rest by name:

- a top-level `list<float>` (or `list<double>` is **not** read: the stored vectors are `f32`);
  a `REQUIRED` list of `REQUIRED` items, or `OPTIONAL` with no nulls (M84's check, applied to
  both levels);
- every row's list has the same length `D`: found by decoding the **repetition levels** once per
  page (a repetition level of 0 starts a row; the run lengths between are `D`), and refusing a
  page whose rows differ;
- uncompressed v1 pages and v2 pages (M84's rules); compressed, dictionary-encoded and nulls
  stay refused (nulls inside a list, or null lists, change the value count).

## Acceptance criteria

1. `a_list_of_floats_column_reads_back_every_row`: a pyarrow `list<float32>` file with equal-length
   lists (v1 and v2, several pages) locates with `dims = D`, every row equal to the generator's
   floats.
2. `a_list_column_that_cannot_be_read_in_place_is_refused_by_name`: unequal lengths, a null list, a
   null item, `list<double>`, a nested `list<list<float>>` and a compressed page are each
   refused with their reason.
3. `repetition_levels_give_the_row_length` (unit): the run decoding at both bit widths.
4. `an_index_over_a_list_column_finds_the_exact_neighbours`.
5. **Gates:** `./scripts/gates.sh`; the mutation sweep of the changed lines misses 0.

**Size:** M. Reuses M84's `present_runs` for the definition levels and adds a repetition-level
decoder and a per-page row-length check.
