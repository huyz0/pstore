# M82 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, real pylance 13 datasets
(`crates/pstore-format/tests/data/generate_lance.py`) plus a protobuf builder in the test.

Commands: `cargo test -p pstore-format --test lance`: 7 tests; `cargo test -p pstore-index --test
external`: 12 tests.

1. `a_lance_dataset_reads_back_every_row`: the pylance dataset `one` (2,600 rows, 8 dimensions)
   pins, is bound to a key under `/data/`, and every row equals the generator's floats; one row's
   range is 32 bytes.
2. `a_column_that_cannot_be_read_in_place_is_refused_by_name`: `many` ("3 fragments"), `deleted`
   ("deleted rows"), `nullable` ("null-free float32") and `v21` ("format 2.1") are refused with
   those reasons; a missing column and the `id` column are refused.
3. `several_pages_are_one_extent_each`: three synthetic pages of 3, 4 and 5 rows give extents
   `(0,3) (3,4) (7,5)` and all twelve rows read back. ⚠️ pylance writes one page at this size even
   through its low-level writer, so the multi-page path has no real-file fixture.
4. `a_page_table_that_lies_is_refused`: wrong sizes (31, 33, zero rows), an offset past the file,
   zero or two buffers, 64 bits, a buffer index or type, a dimension of 3, another type URL, and
   row counts that do not sum are each refused by name.
5. `a_manifest_or_file_that_is_not_lance_is_refused` and
   `the_edges_of_what_the_file_may_hold_are_exact`: bad magic, footer and manifest versions, a
   wrong file size, escaping paths, other logical types, truncated and missing objects; fixed-width
   protobuf fields are stepped over; a last page ending exactly at the file's end is accepted and
   one byte more is not; column metadata of exactly the 8 MiB cap is read, one more is refused,
   and a claim running exactly to the file's end is inside it.
6. `a_changed_data_file_is_noticed`: a data file edited after pinning is `Changed`.
7. `an_index_over_a_lance_dataset_finds_the_exact_neighbours`: build, reopen (blob byte-identical)
   and search at a full window equal the exact cosine top 10 for three queries; a changed data
   file is refused.
8. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants`, `--no-config`, `lance.rs` and `external.rs`): first sweep 119 mutants, 111
   caught, 6 unviable, 2 missed (the metadata end boundary); a test was added and a re-run of every
   data-file-locator mutant gives **41 mutants, 40 caught, 1 unviable, 0 missed**. An earlier sweep's
   other misses (fixed-width skips, buffer index and type, page-end and metadata-cap boundaries)
   are killed by the tests above. ⚠️ `cargo deny check advisories` still fails on `paste` (via
   `foyer`), as before.

**Not covered:** several fragments or data files; deleted rows; Lance 2.1+; compressed or nullable
vector columns; choosing the latest manifest; a `data/` directory outside the dataset's tree.
