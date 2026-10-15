# M88 — Parquet columns that hold nulls (PLANNED, not built)

**Serves:** backlog row 74's remainder, after [M84](../M84/SPEC.md). **Status: planned.** No owner
decision is needed; the design choice is made here.

## Decision

A null row has no vector, so it cannot be a position's vector. As M87 does for deletions, a null
row **keeps its number and is excluded from every answer**: the definition levels M84 reads once
become a per-column **validity bitmap** (one bit a row) stored in the codes blob, and the values of
a page with nulls are `present × width` bytes, so a row's offset is the page start plus
`rank(row) × width` (the count of present rows before it). A page with no nulls is unchanged.

- Reads: a page with nulls costs a rank lookup on the bitmap, held in memory with the codes.
- `search` drops null rows; `read_rows` of a null row is an error.
- Still refused: nested/repeated columns, a maximum definition level above 1, dictionary-encoded
  or compressed-v1 optional pages (their levels are not reachable by a computed range).

## Acceptance criteria

1. `a_column_with_nulls_reads_back_every_present_row`: the real `optional-nulls.parquet` (every
   tenth value null) pins; every present row equals the generator's floats and no null row is
   returned by any search.
2. `rank_gives_the_position_of_every_present_row` (unit, exhaustive over small bitmaps).
3. `a_null_row_is_an_error_to_read`.
4. `the_codes_blob_round_trips_the_bitmap` (`PXI6` with M87's, or its own revision).
5. `an_index_over_a_column_with_nulls_finds_the_exact_neighbours`.
6. **Gates:** `./scripts/gates.sh`; mutation sweep of the changed lines misses 0.
