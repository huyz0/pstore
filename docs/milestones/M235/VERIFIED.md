# M235 verification

How to run the tests: `cargo test -p pstore-format --test it parquet_write`,
`cargo test -p pstore-format --lib parquet::write`, and
`cargo test -p pstore-server --test it -- parquet_export openapi`. The measurement:
`cargo test -p pstore-server --test it -- --ignored export_cost --nocapture`.

Red runs: the tests name `pstore_format::parquet::write` and the `export.parquet` route, which
the parent does not have, so they were red by not compiling. After that,
`a_field_named_as_an_attribute_is_left_out_by_name` was seen red with the name check removed,
`the_parquet_export_pages_one_epoch` with the page's extra row (`limit + 1`) removed, and the
fixture steps red until the contract named the operation.

`cargo mutants`, scoped to `pstore-format`'s tests for the writer and to the server's
`parquet_export` and `openapi` tests for the handler: the first sweep of
`parquet/write.rs` left 13 missed and 11 timeouts. The timeouts were the reference reader
looping on a broken file; each read now runs on a thread bounded at 30 s, so a hang fails.
The misses were dead arithmetic (a value count no page read, a level bit width that only
mattered as zero or not: both removed), bits combined with `|` that `^` could not change (now
`+`), a kind table the test derived with the code under test (now stated), no empty vector, no
byte-exact Thrift check (three unit tests now), and no level run past 63 (a long-runs test).
The second sweep: 94 tested, 89 caught, 2 unviable, 3 missed, all in the run header's second
byte; with the long-runs test those 6 mutants are caught. The handler's 7 mutants: 7 caught.

1. `a_page_reads_back_through_the_parquet_crate`: green. Pages of 1, 37 and 1,000 seeded rows
   over every `Value` kind, absent values, mixed kinds under one name (`Json`), ints and
   floats under one (`Double`), arrays, empty strings, `i64::MIN`/`MAX`, uint64 above
   `i64::MAX`, `-0.0`, empty and absent vectors and a named field: the `parquet` crate reads
   every column's value back as written, and each attribute's kind is the stated one. With
   `a_field_of_several_vectors_a_row_is_left_out_by_name`,
   `a_field_named_as_an_attribute_is_left_out_by_name`, `an_empty_page_is_a_file_of_no_rows`,
   `long_runs_of_one_level_read_back`, and the Thrift unit tests
   `integers_are_zigzag_varints`, `a_field_header_is_its_delta_and_type_or_its_id_in_full`,
   `a_list_header_is_short_below_fifteen_items`: green.
2. `the_parquet_export_pages_one_epoch`: green. 25 rows folded at `E`, paged 10 at a time by
   `after` while a delete, three new rows and two unfolded writes (one replacing a row of `E`)
   land: the pages hold the 25 rows of `E` once each, in id order, with their attributes and no
   vectors; the epoch and next `after` are in the headers and the file's metadata, absent on
   the last page and on an exact last page; the present epoch's page holds the delete and the
   new rows, with vectors when asked.
3. `the_parquet_export_pages_one_epoch` (refusals): `limit` 0, 10,001, and 1,001 with vectors;
   `include_vectors=yes`; `as_of` not a number or past the present; an absent index (`404`).
   The contract's own steps in `api/fixtures/native.json` (`limit=0`, `include_vectors=maybe`)
   hold against the server and the schema in `the_fixtures_hold_against_the_server_and_the_contract`.
4. `export_cost`, `provisional` (in-memory store, a debug build, one 4-core container, best of
   5). 20,000 rows, 64-dimension vectors, five attributes (int, float, string, bool, title):

   | page | Parquet bytes / ms | JSON export (M199) bytes / ms |
   |---|---|---|
   | 10,000 rows, attributes | 598,743 / 127.8 | 1,113,240 / 166.4 |
   | 1,000 rows, attributes and vectors | 319,523 / 35.3 | 794,508 / 56.3 |

   A Parquet page is 46% and 60% smaller, and 23% and 37% faster, than the same JSON page;
   both read the same segments (one ordered read, the same requests).

Downsides, measured or by construction: (a) **uncompressed** PLAIN pages, no dictionary: a
consumer that keeps the files compresses them on rewrite. (b) **Named vector fields and the
sparse field are not exported** by the endpoint (the writer leaves out a field of several
vectors a row, or one sharing an attribute's name, by name). (c) **A page is bounded like a
JSON page** (10,000 rows, 1,000 with vectors), so a large index is many files. (d) A column's
type is chosen per page: an attribute holding only ints in one page and a float in the next is
`INT64` in one file and `DOUBLE` in the other, which a reader of the dataset must unify. No
setting: it is a request and costs nothing when not asked. `cargo deny`: licences and bans
pass with `parquet` (a dev-dependency only); advisories fail on three quinn advisories that
fail identically on the parent branch.

Gates, in a cloud container: GATES-PENDING
