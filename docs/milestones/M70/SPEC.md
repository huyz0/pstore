# M70 — Parquet files as an external column

**Serves:** OQ-169, after [M68](../M68/SPEC.md) (the pinned read) and [M69](../M69/SPEC.md) (the
index). Both took `base`, `stride` and `rows` from the caller. This milestone **finds them in a
real Parquet file's footer**, so a customer names a file and a column and nothing else.

## What qualifies

A Parquet column whose values a ranged read can reach: top-level, `REQUIRED`,
`FIXED_LEN_BYTE_ARRAY(dims × 4)` holding little-endian `f32`, `UNCOMPRESSED`, `PLAIN`-encoded,
no dictionary page, v1 data pages. Anything else is **refused with the reason**: a compressed
column needs a decompress, a dictionary a lookup, an optional column definition levels, and a
`LIST<FLOAT>` repetition levels -- none is a computable range. (The writer's knobs that give the
qualifying shape: `compression=NONE`, `use_dictionary=False`, a `required` field.)

## Delta

1. **`pstore_format::parquet::locate(store, key, column)`:** reads the footer (the 8-byte tail,
   then the metadata), parses it with a Thrift-compact reader written here, checks the column
   qualifies, then walks each row group's column chunk **page header by page header** (one
   small ranged read per page, at build time only) and returns the object's length, `dims`, and
   one `Extent { first_row, rows, offset }` per data page.
2. **`ExternalColumn` gains `extents`.** Empty means M68's single run (`base`, `stride`,
   `rows`). Otherwise row `r`'s vector is at its extent's `offset + (r - first_row) × dims × 4`.
   `ExternalColumn::pin_parquet(store, key, column)` pins the located extents: every extent must
   lie inside the object, rows must be contiguous from 0, and the pin's length and tag guard
   changes as before.
3. **The codes blob carries the extents** (`PXI2`), so `ExternalIndex::build`, `open` and
   `search` work over a Parquet column unchanged.

**Not covered:** Lance manifests (Lance's v2 files are protobuf and are read by a caller who
supplies the single run, as M69), compressed or dictionary columns, `LIST<FLOAT>` columns,
nested columns, v2 data pages, clustering, filters, HEAD naming the index, and an engine/API
surface.

## Acceptance criteria

1. `locate_finds_the_real_files_vectors`: against real Parquet written by pyarrow 26
   (`tests/data/good.parquet`, 3 row groups of 100 rows, and `multipage.parquet`, 2,600 rows in
   several pages), the located extents read back **exactly the floats the generator wrote**
   (`*.vectors.f32`), for every row, including rows on page and row-group boundaries.
2. `locate_refuses_what_cannot_be_reached`: the snappy, dictionary, optional and
   `LIST<FLOAT>` files, a missing column, a truncated file, a non-Parquet file and a corrupt
   footer are each refused, the first four naming why.
3. `varints_and_zigzag`, `fields_lists_and_skips`, `bools_and_nesting_depth`: unit tests of the compact reader: varints and zigzag,
   field deltas and long-form ids, lists (short and long form), nested structs, skipping every
   type.
4. `extents_address_rows`: `row_range` over several extents, at the first and last row of each,
   past the end, and over a gap-free boundary; `pin_parquet` refuses extents outside the
   object, overlapping or gapped rows, and zero-row extents.
5. `an_index_over_a_parquet_file_finds_the_exact_neighbours`: build, reopen and search the 2,600-row
   `multipage.parquet` written by pyarrow, results equal an exact cosine over the generator's floats at
   `window` = all rows, and a changed object is refused.
6. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the new code misses 0, or each
   miss is killed by a named test.
