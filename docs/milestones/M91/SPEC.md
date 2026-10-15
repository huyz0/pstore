# M91 — Parquet `list<float>` vector columns

**Serves:** backlog row 74's last item ("nested columns"). Scope is narrow on purpose.

## What "nested" means here

The vector columns real pipelines write as Parquet are `list<float>` (the repo's own `list.parquet`
fixture), not `FIXED_LEN_BYTE_ARRAY`: a top-level group holding a repeated group holding a `float`
leaf, with a repetition and a definition level a value. If every list has the same length `D` and
no list or item is null, a page's values are `rows × D` contiguous `f32` behind two level streams:
the shape [M84](../M84/SPEC.md) reads, with two streams instead of one.

## What is read, and refused

- **Read:** `OPTIONAL` or `REQUIRED` list of `OPTIONAL` or `REQUIRED` float items, plain,
  uncompressed, v1 and v2 pages, any number of row groups. `D` is **found**, not declared: the
  first page's repetition levels give the first list's length, and every list of every page and
  row group must match it.
- Each page's levels are read once and checked: every definition level is the maximum (no null
  list, no null item); the repetition levels begin a row and every row is `D` long, so no list
  spans a page. The located extents are contiguous floats only.
- **Refused, by name:** a null list or null item; lists of different lengths or a list that spans a
  page; compressed pages; a dictionary page; `list<double>` and other element types; a list nested
  deeper (`list<list<float>>`); a `REPEATED` leaf directly.
- The schema walk now finds a column by its **top-level** element and matches a chunk by its whole
  `path_in_schema`, so a nested leaf named like a top-level column can no longer be mistaken for it.
  `list.parquet`, which was refused in M70 for its repetition levels, now reads: a deliberate
  change of behaviour recorded here, not a weakened test.
- ⚠️ Bounded: a page claims at most 2^26 values; level streams are checked without allocating per
  value; a column splits into at most 2^22 extents.

## Acceptance criteria

1. `lists_of_floats_read_back_every_row`: pyarrow-written `list-v1`, `list-v2`, `list-required`
   (non-null items) and `list-groups` (three row groups) files (2,600 rows of 8 floats, several
   pages) locate with `dims = 8` and every row equals the generator's floats; `list.parquet`
   (300 rows) too.
2. `a_list_column_that_cannot_be_read_in_place_is_refused_by_name`: snappy, double, ragged, null
   list, null item and nested-list files are each refused with their reason.
3. `levels_decode_in_rle_and_bit_packed_runs`, `repetition_levels_give_the_row_length`,
   `definition_levels_must_all_be_the_maximum` and `bit_widths_are_those_of_the_largest_level`
   (unit): the level decoding at widths 1 and 2, the row-length rule, the null rule.
4. `an_index_over_each_codec_finds_the_exact_neighbours`: `list-v1`, `list-v2` and `list-groups`
   join the other files and match the exact cosine top 10 at a full window.
5. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0.

**Not covered:** compressed list pages; dictionary-encoded lists; `list<double>`; lists of
varying length (a different column, a ragged one).
