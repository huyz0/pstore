# M90 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, real pylance 13 datasets
(`tests/data/lance21/`, written by `generate_lance21.py`). **The layout was read from one writer**;
its rules were checked against 130 shapes of (rows, dimensions) and held for all of them.

Command: `cargo test -p pstore-format --test lance21`: 7 tests.
**Red first:** before the reader, `v21` was refused as "format 2.1", which is what the old
`a_column_that_cannot_be_read_in_place_is_refused_by_name` asserted.

1. `a_lance_2_1_dataset_reads_back_every_row`: 1, 129, 256 and 2,600 rows of 3 or 8 floats
   (one chunk, a padded chunk, two exact chunks, 21 chunks) and 300 rows of 100 (`FullZip`) read
   back every row equal to the generator's floats, in the expected number of runs.
2. `a_2_1_column_with_nulls_is_refused_by_name`: the nullable dataset is refused for its layout.
3. `a_damaged_chunk_index_is_refused_by_name`, `a_page_whose_chunks_do_not_fill_its_buffer_is_refused`,
   `a_chunk_index_of_an_odd_length_is_refused`,
   `a_layout_that_is_not_a_plain_null_free_run_is_refused_by_name` and
   `a_full_zip_page_must_be_exactly_its_rows`: byte-patched copies of the real files, each
   refused with its own reason, including a buffer ending exactly at the end of the file.
4. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants --no-config --in-diff`, `lance.rs` and `lance/v21.rs`): first sweep 88
   mutants, 5 missed (chunk-index bounds that the later checks make redundant, simplified);
   re-sweep: **78 mutants, 70 caught, 8 unviable, 0 missed.**

**Not covered:** nullable columns; compressed values; writers other than pylance 13; a page whose
chunk index is large enough to need `has_large_chunk`.
