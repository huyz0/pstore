# M95 — Attribute columns from the Parquet file filter an external index

**Serves:** backlog row 75. [M73](../M73/SPEC.md) gave `ExternalIndex::search_where` a
caller-supplied row predicate; the attribute data was the caller's. This reads the attribute from
**the same Parquet object the vectors are in**, so a caller names a column and a predicate and
the file supplies the values.

## Delta

1. **A scalar column is located by the machinery a vector column is.** `parquet::locate_scalar`
   finds a top-level `INT32`, `INT64`, `FLOAT` or `DOUBLE` column and returns the same
   `Located` runs: compression (snappy, gzip, brotli, lz4_raw, zstd), dictionary encoding, v1 and
   v2 pages and nulls are read exactly as for a vector, because a value is a 4- or 8-byte "vector"
   of one or two `f32` lanes. It also returns the column's kind. Any other type (`BOOLEAN`,
   `INT96`, `BYTE_ARRAY`, `FIXED_LEN_BYTE_ARRAY`) is refused naming the type; a nested column is
   refused. A column with a **logical type** its physical value does not carry -- unsigned,
   decimal, date, time, timestamp -- is refused as such, because reading a `uint32` of
   3,000,000,000 as an `INT32` would hand back a negative number and a filter would match the
   wrong rows; only an absent logical type or a signed integer one is read. `locate` (a vector
   column) is unchanged.
2. **`ExternalAttribute`** (`pstore-format::external`): a pinned scalar column.
   `ExternalAttribute::pin_parquet(store, key, column)` and
   `read_all(store) -> Vec<Option<AttrValue>>`: one entry per row, `None` for a null.
   `AttrValue` is `Int(i64)` (INT32 and INT64) or `Float(f64)` (FLOAT and DOUBLE, a `FLOAT`
   widened exactly). The values are the file's bits, never rounded. `read_all` refuses a column
   of more than `MAX_ATTRIBUTE_ROWS` (2^27) rows: it holds the column and a mask in memory.
3. **`ExternalIndex::search_attr(store, query, k, window, attr, keep)`**: reads `attr`, admits
   row `r` when `keep(&value_r)` (a null row is `None`), and runs `search_where` over the
   admitted rows. The attribute must be in **the index's own object** and have the index's row
   count; anything else is refused before a vector moves, because a row map against another
   object would filter the wrong rows silently.

## Acceptance criteria

1. `a_scalar_column_is_located_and_read_back` (`pstore-format`, `--test parquet`): the `id`
   column (`INT64`, `0..rows`) of `good.parquet` (300 rows) and `multipage.parquet` (2,600 rows,
   several pages) reads back as `Int(row)` for every row; `read_all` is one call and its answer
   is exact.
2. `a_scalar_column_is_read_in_every_encoding`: the same column, `Int(row)` for every row, in
   each of the 24 flat fixtures of M70-M91 that can be read: every codec, dictionary, v2 pages and
   nullable-schema columns. The two the vector column cannot be read in either
   (`optional-snappy`, `optional-dictionary`) are refused for the same reasons.
3. `scalar_columns_of_each_type_read_back_with_their_nulls` (`scalars.parquet`, written by
   `generate_scalars.py`): `INT32`, `INT64`, `FLOAT`, `DOUBLE` and `INT_8`, and three columns
   with **real nulls** (every 7th row), read back exactly with `None` at the nulls.
   `a_scalar_column_of_each_kind_decodes`: the decoder over hand-built bytes: `INT32` extremes,
   `INT64` extremes, `FLOAT` widened exactly, `DOUBLE` bit-for-bit including a subnormal and
   `-0.0`, and the wrong width is no value.
4. `a_non_scalar_column_is_refused_with_its_type` and
   `a_logical_type_its_physical_value_does_not_carry_is_refused`: `BOOLEAN`, `BYTE_ARRAY`,
   `INT96` and `FIXED_LEN_BYTE_ARRAY` are refused with their type named, a list as nested, a
   missing column as such, and `uint32`, `uint64`, `date32`, timestamps (`ms`, `ns`) and
   `decimal` as a logical type; `locate` of `id` still says `FIXED_LEN_BYTE_ARRAY`.
   `nulls_that_cannot_be_read_in_place_are_refused_by_name` (a v1 compressed page, a v2
   compressed page that holds nulls, a column of only nulls) and
   `a_column_claiming_too_many_rows_is_not_read`.
5. `a_filter_on_the_files_own_column_equals_the_exact_filtered_cosine` (`pstore-index`,
   `--test external`): on the 2,600-row fixture, `search_attr` with `id % 7 == 3` and a full
   window returns the exact filtered cosine top 10, every row satisfies the predicate, and a
   window of 20 still returns 10 rows.
6. `an_attribute_of_another_object_or_length_is_refused`: an attribute pinned in a different
   object, and one with a different row count, are refused with `Corrupt`, and **no byte** of any
   object moved (an accounted store).
   `a_null_attribute_reaches_keep_as_none`: over `vectors-attrs.parquet` (vectors beside an
   `INT32` with nulls and a `DOUBLE`), `keep` sees `None` for the nulls, and four predicates
   each equal the exact filtered cosine.
7. `an_attribute_that_admits_nothing_reads_no_vector`: `keep` false for every row returns no row
   and moves no byte of the vector column's pages.
8. **Gates:** `./scripts/gates.sh` passes; the coverage floors hold; the mutation sweep of the
   changed lines misses 0 (an equivalent mutant is argued, never skipped).

## Not covered

- Lance datasets' scalar columns; string, boolean, unsigned, decimal and temporal attributes
  (`BYTE_ARRAY` needs a length-prefixed decode; the others need their logical type applied).
- A **compressed page that holds a null**, and a column of nothing but nulls: refused, as for
  a vector column (M84). pyarrow's default writer compresses, so a nullable attribute written
  with nulls is read in place only from raw pages.
- A predicate language over several attributes: the caller's `keep` takes one value. A caller
  combines columns by calling `search_where` with a mask it built from several
  `ExternalAttribute::read_all`s.
- Reading **only the pages a predicate could match** (a zone map over the footer's statistics):
  `read_all` reads the whole attribute column, `8 · rows` bytes, once per search. Against the
  vector column's own `dims · 4 · rows` this is a minor fraction, and it is named here because it
  scales with rows.
- An engine or API surface (backlog row 76).
