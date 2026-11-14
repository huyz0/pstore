# M235: Parquet export of an index's rows

## Serves
Item 6 of [the column-storage adoption list](../../comparisons/column-storage-adoption.md):
"interoperability with DuckDB, Spark and DataFusion without a pstore client". D-6's promised
export path. Builds on [M199](../M199/SPEC.md)'s export at one epoch, which pages JSON.

## Delta
Today an export is M199's loop: page `rank_by ["id", "asc"]` at `as_of E`, as JSON. An
analytics engine needs a client to turn the pages into a table.

After, **`GET /v1/indexes/{index}/export.parquet`** answers one page of the same loop as a
Parquet file (`application/vnd.apache.parquet`): `as_of` (default the present HEAD's epoch,
which the response names), `after` (the last id of the previous page), `limit` (rows, at most
100,000, default 10,000) and `include_vectors`. A page is a whole Parquet file with one row
group, so a client saves each and reads them as one dataset. The response's
`x-pstore-epoch` and `x-pstore-next-after` (absent on the last page) drive the loop.

The file, written by a writer of our own in `pstore-format` (`parquet::write`, the reverse of
M70's footer reader, no new runtime dependency):
- `id`: `BYTE_ARRAY` `UTF8`, required, the rows in id order.
- One column per attribute name the page holds, in name order, optional (absent is null),
  typed from its values: all `Int` as `INT64`; all `Float` (or `Int` and `Float`) as
  `DOUBLE`; `Bool` as `BOOLEAN`; `Str` as `BYTE_ARRAY` `UTF8`; `DateTime` as `INT64`
  `TIMESTAMP(MICROS, UTC)`; `Uint` as `INT64` `UINT_64`; `Uuid` as
  `FIXED_LEN_BYTE_ARRAY(16)` `UUID`; anything else (bytes, arrays, mixed kinds) as
  `BYTE_ARRAY` `JSON`, the value as the API writes it.
- With `include_vectors`, `vector` as a required list of `FLOAT`, and each named dense field
  likewise (optional: a row may lack one).
- `PLAIN` encoding, uncompressed, v1 data pages, one page a column chunk; the file's
  key-value metadata names the index, the epoch and the pstore version.

Not delivered: writing to a customer's bucket (the client stores the pages), the sparse
field, compression and dictionary encoding (a consumer compresses on rewrite).

No setting: it is a request, and costs nothing when not asked.

## Acceptance criteria
1. Read back by an independent reader: for seeded documents over every `Value` kind,
   absent attributes, mixed kinds under one name, empty strings, extreme ints, uint64 above
   `i64::MAX`, `-0.0`, NaN, and vectors, the official `parquet` crate (a dev-dependency only)
   reads every page's schema and every value back as written.
2. The loop: paging an index with deletes, a replacement and unfolded rows written after `E`
   by `after` and `limit` returns, across all pages, exactly the rows M199's JSON export
   returns at `E`, each once; the last page names no next.
3. Refusals by name: a `limit` of 0 or over 100,000, an `as_of` past the present or below the
   reap horizon, an absent index (`404`).
4. Cost recorded in `VERIFIED.md` (`provisional`): bytes and time of a 10,000-row page at
   20,000 rows, against the same page as JSON.

## Test plan
- 1: `a_page_reads_back_through_the_parquet_crate` (pstore-format `tests/it/parquet_write.rs`)
  and unit tests of the Thrift writer. Kills: a definition level inverted; a column's type
  chosen from the first value only; a page header's size off by one.
- 2, 3: `the_parquet_export_pages_one_epoch` (server `tests/it/parquet_export.rs`).
- 4: `scripts/columns.sh` or the server test's measurement.

## RA budget
A page is M199's ordered read: three rounds, the same requests as a JSON page.

## Risks
- A Parquet writer we own can write a file a strict reader refuses: criterion 1 reads every
  file with the reference implementation.
- `parquet` as a dev-dependency must pass `cargo deny`.

## Tasks
- M235.1: `parquet::write`, the endpoint, OpenAPI and clients, tests, VERIFIED.md.
