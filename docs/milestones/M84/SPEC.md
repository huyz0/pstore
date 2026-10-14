# M84 — OPTIONAL Parquet columns that hold no nulls

**Serves:** backlog row 74's remainder ("nullable"), after [M75](../M75/SPEC.md)–[M81](../M81/SPEC.md).
pyarrow writes a column **nullable by default**, so until now `optional.parquet`-shaped files —
the common case — were refused even when they held no nulls at all.

## What is read

An `OPTIONAL` flat `FIXED_LEN_BYTE_ARRAY(dims × 4)` column carries one definition level (bit
width 1) a value. If every level of every page says "present", the values are exactly a
`REQUIRED` column's, behind the levels; so the levels are read once at locate time, found to be
all "present", and stepped over: each extent starts after them.

- **v1 pages:** the body is `[length: u32][levels][values]`. Read only when the page is
  uncompressed, because the levels sit inside the compressed body otherwise.
- **v2 pages:** the levels sit before the (possibly compressed) values and the header says how
  long they are; compressed v2 pages work, and `nulls` in the header must be 0.
- ⚠️ **Refused, with the reason:** any level that says "null" (`holds nulls`); a compressed v1
  page; a dictionary-encoded optional column (its index pages carry levels); `REPEATED`; levels
  longer than their page, empty, or followed by stray bytes. The level check allocates nothing
  proportional to the values, and a page claiming more than 256 MiB of values is refused first.
- A `REQUIRED` column is read exactly as before.

## Acceptance criteria

1. `optional_files_without_nulls_read_back_every_row`: pyarrow-written `optional.parquet` (three
   row groups), `optional-multipage.parquet` (v1), `optional-v2.parquet` and
   `optional-v2-zstd.parquet` (2,600 rows, several pages) each locate and every row equals the
   generator's floats.
2. `an_optional_column_that_cannot_be_read_in_place_is_refused_by_name`: `optional-nulls.parquet`
   (every tenth value null) is `holds nulls`; `optional-snappy.parquet` (v1, compressed) names
   the compressed body.
3. `damaged_definition_levels_are_refused`: a level byte flipped to null, a length prefix that
   runs past the page, and a zero length are each refused by name.
4. `levels_that_say_present_are_checked_exactly` (unit): RLE and bit-packed runs, mixed runs, a
   two-byte run header, bits of padding that are not read, a null inside the values, a run of
   nothing, a header that never ends, trailing bytes and truncation.
5. `locate_refuses_each_shape_it_cannot_read`: `REPEATED` is refused; an `OPTIONAL` schema over
   pages with no levels is refused by name (`optional.parquet` moved from the refusals to
   criterion 1: a deliberate change of behaviour, not a weakened test).
6. `an_index_over_each_codec_finds_the_exact_neighbours`: the three optional files join the
   other codecs and match the exact cosine top 10 at a full window.
7. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0, or
   each miss is killed by a named test.

**Not covered:** nulls (a row with no vector); dictionary-encoded or compressed-v1 optional
columns; nested columns; maximum definition levels above 1.
