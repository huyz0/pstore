# M100 — Boolean and string attribute columns

**Serves:** backlog row 75. After [M99](../M99/SPEC.md), a Parquet attribute column can be any
fixed-width number or instant. Two kinds are still refused, and they are the ones a filter most
often names:
- `BOOLEAN`, which is bit-packed;
- `BYTE_ARRAY` strings, which carry a length before each value.

Neither is a fixed width a row, so M95's extents (row r at `offset + rank × width`) cannot reach
one. Lance scalar columns are **M101**. Zone maps are later.

## What is true today

These facts come from reading the tree after M99. Nothing here was measured.

- `scalar_kind` refuses `BOOLEAN` and `BYTE_ARRAY` by name.
- **What `ExternalAttribute` is.** It is `{ column: ExternalColumn, kind }`. Its `read_all` reads
  every present row through `column.read_rows_bytes`, and decodes each as `kind.width()` bytes.
- **What `pstore-index` reads of it.** `search_attr` reads only `attr.column.key` and
  `attr.column.rows` from the attribute (to refuse another object or length), then `read_all`.
- **What can already be decoded.** The page header, every codec through `external::decompress`,
  and definition levels at width 1 (`present_runs`).
- **What is missing.** A general RLE/bit-packed hybrid at widths up to 32, which dictionary
  indices need.
- **The fixture.** `scalars.parquet` holds `flag` (bool) and `name` (string), both PLAIN, with no
  nulls and no dictionary.

## Delta

1. **Kinds and values.**
   - `AttrKind` gains `Bool` (physical `BOOLEAN`, no logical type) and `Str`.
   - `Str` is `BYTE_ARRAY` with the logical type `STRING` (union field 1) or the converted type
     `UTF8` (0).
   - A `BYTE_ARRAY` with no logical type, or with any other one (`JSON`, `ENUM`, `BSON`,
     decimal …), is still refused, naming it a binary column. Its bytes are not text this crate
     can compare.
   - `AttrValue` gains `Bool(bool)` and `Str(String)`, so it is no longer `Copy`.
   - `as_f64` of a `Bool` is 0 or 1. Of a `Str` it is NaN, which compares to nothing, and it is
     documented so.
   - `width()` is `None` for both new kinds, so it becomes `Option<usize>`. `decode` of either
     is `None`.
2. **Where an attribute is, by kind.**
   - `ExternalAttribute` becomes `{ kind, source: AttrSource }`. `AttrSource` is one of:
     - `Fixed(ExternalColumn)`, which is M95's and M99's column, unchanged;
     - `Chunks(AttrChunks)`, which is `{ key, len, tag, rows, chunks: Vec<AttrChunk> }`.
   - An `AttrChunk` is a row group's column chunk: `{ first_row, rows, offset, len, codec,
     optional, dictionary }`. It runs from the dictionary page, if any, to the chunk's end, and
     `dictionary` says the footer declared one: such a chunk must begin with it.
   - `key()` and `rows()` give the same answers for both sources. `search_attr` uses them in
     place of `attr.column.*`.
3. **The pin.**
   - `pin_parquet` of a `Bool` or `Str` column reads the footer only. It records each chunk's
     range, with the same checks M70 to M88 make of a chunk:
     - the codec is one this crate reads;
     - the chunk lies inside the file;
     - `num_values` equals the row group's rows;
     - the column is not `REPEATED`.
   - It reads no page. A page is judged when it is read.
4. **`read_all` of `Chunks`.**
   - It checks the object, as `read_rows_bytes` does: `Changed` if its length or tag moved.
   - It makes one ranged read a chunk, all chunks in one round (fan-out).
   - It walks each chunk's pages:
     - It decompresses each with the chunk's codec. For a v2 page, it decompresses only the
       values, and only when `is_compressed`.
     - It reads the definition levels at width 1. A v1 page has a 4-byte length; a v2 page uses
       `definition_levels_byte_length`.
     - It decodes the values:
       - `Bool`: `PLAIN` (bit-packed, least significant bit first), or `RLE` (a 4-byte length,
         then the hybrid at width 1);
       - `Str`: `PLAIN` (a 4-byte little-endian length, then the bytes), or
         `PLAIN_DICTIONARY` / `RLE_DICTIONARY` (one byte of bit width, then the hybrid). The
         indices go into the chunk's `PLAIN` dictionary page.
     - A chunk may switch from its dictionary to `PLAIN` partway (pyarrow's fallback). Each
       page is decoded by its own encoding.
   - **Refused, `Damaged`:** any other encoding, a page or value running past its page, levels
     or values that disagree with the page's count, an index past the dictionary, and a string
     that is not UTF-8.
   - **Bounded:** a page's uncompressed size is at most `MAX_PAGE_BYTES`, as today. The strings
     of one `read_all` hold at most `MAX_ATTRIBUTE_BYTES` = 1 GiB, and more is `Shape`. So do the
     chunks the footer claims, checked before any is fetched. Rows are
     capped by `MAX_ATTRIBUTE_ROWS`, as today.
5. **The hybrid decoder.** A new `parquet::hybrid(bytes, n, width) -> Option<Vec<u32>>`
   handles widths from 0 to 32, with M91's `each_level` rules: exactly `n` values, and no bytes
   left over. A width of 0 means every value is 0, which is a dictionary of one entry. An RLE
   run whose value is wider than `width` is refused.

**Not changed:** the fixed-width path; `pstore-index`, apart from the two accessor calls; the
engine and the API (row 76).

## Acceptance criteria

1. **Read back in every layout.** `bool_and_string_columns_read_back_in_every_layout`
   (`crates/pstore-format/tests/parquet.rs`) reads four columns from three new pyarrow 26
   fixtures, row for row:
   - `name` (`s{r}`), and `namen`, which is null every 7th row;
   - `flag` (r odd), and `flagn`, which is null every 7th row.

   Each fixture has 300 rows in 3 row groups, with pages of at most 512 bytes:
   - `strings-plain.parquet`: no dictionary, no compression, v1 pages;
   - `strings-dict-snappy.parquet`: a dictionary, snappy, v1 pages. Its `long` column (a
     distinct 300-byte string a row, with a 4 KiB dictionary page limit) falls back to PLAIN
     partway through a chunk. The test asserts that each `long` chunk declares a dictionary,
     which holds at most 13 of its 100 distinct strings, so the rows read back came mostly from
     PLAIN pages after it.
   - `strings-v2-zstd.parquet`: a dictionary, zstd, v2 pages.

   `scalars.parquet`'s `flag` and `name` read back too, and so do `scalars-zstd.parquet`'s and
   `scalars-v2-zstd.parquet`'s.
2. **What is refused, by name.**
   - `bin` (binary, no logical type) is refused at the pin, naming a binary column.
   - `dec`, `scalars-typed.parquet`'s `time` and `int96.parquet`'s `ts96` stay refused, as in
     M99.
3. **Damage is `Damaged`, never a wrong value.** In `a_damaged_chunk_is_refused_not_misread`,
   one hand-edited copy of `strings-plain.parquet` each:
   - a string's length running past its page;
   - an invalid UTF-8 byte;
   - a dictionary index past the dictionary (in a copy of the dictionary file);
   - an object changed after the pin, which gives `Changed`.
4. **The hybrid.** `the_hybrid_decodes_every_width` (a unit test) covers:
   - widths 0, 1, 3, 8, 17 and 32, in both run kinds and mixed;
   - a stream short by a byte, a stream with a byte left over, and a width of 33, each `None`.
5. **Bounds.** `a_column_too_large_to_hold_is_not_read` (a unit test). The byte cap is a
   parameter of the inner decode that `read_all` calls with `MAX_ATTRIBUTE_BYTES`, and the test
   calls it with a small cap:
   - strings of exactly the cap are read;
   - one byte more is `Shape`.

   `a_column_whose_chunks_are_too_large_to_fetch_is_not_read` (`tests/parquet.rs`): chunks
   claiming one byte past the cap are `Shape` before any fetch, and at the cap they are not.
6. **An external filter on a string.**
   `a_filter_on_a_string_column_equals_the_exact_filtered_cosine`
   (`crates/pstore-index/tests/external.rs`):
   - A new `vectors-labels.parquet` holds `good.vectors.f32`'s rows beside `label`
     (`"even"` / `"odd"`, null every 7th row) and `flag`, dictionary-encoded and snappy.
   - Keeping `Str("odd")` equals the exact filtered cosine at a full window, and so does
     keeping `Bool(true)`.
   - M95's and M99's attribute tests still pass, with only their access path changed
     (`attr.column` becomes `attr.source`'s `Fixed`).
7. **Gates.**
   - Each new test is seen red first.
   - `./scripts/gates.sh` passes, and so do the coverage floors (95/95).
   - `./scripts/mutants.sh` misses 0 on the changed lines, or each miss is argued equivalent.

## Test plan

| Test | Catches (mutation) |
|---|---|
| `bool_and_string_columns_read_back_in_every_layout` | bit order of a bool; the v1/v2 level split; a codec applied to v2 levels; the dictionary fallback; a null misplaced |
| `a_damaged_chunk_is_refused_not_misread` | a bounds check removed; UTF-8 unchecked; the object check removed |
| `the_hybrid_decodes_every_width` | the run header parsed wrong; a width's byte count; leftovers accepted |
| `a_column_too_large_to_hold_is_not_read` | the byte cap removed or off by one |
| `a_filter_on_a_string_column_…` | the end-to-end path; the accessors |

## RA budget

- **The pin:** the footer, as today. No new request.
- **`read_all`:** one GET a row group, all in one round, plus the `head` and `get_tag` that
  `read_rows_bytes` already makes.
- These reads scale with the row groups, which scale with bytes. It is no more than M95, which
  reads every page of the column too.
- It is off the hot path: an attribute is read once a filtered search, exactly as M95's is.

## Risks

- **A whole chunk is read**, levels and all, where M95 read only the values' ranges. The bytes
  are the column's own, so it costs at most the levels and headers more.
- **`AttrValue` is no longer `Copy`.** No caller outside the workspace exists.
- **Strings are compared as Rust strings.** That is bytewise, with no collation, as
  `pstore_format::Value::Str` is.

## Tasks

- **M100.1** The `hybrid` decoder and its unit test.
- **M100.2** The kinds, `AttrSource`, the pin and `read_all`, the fixtures and the tests.
- **M100.3** Review, the gates, the ledger, and the backlog row.
