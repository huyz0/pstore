# M87 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, real pylance 13 datasets
(`crates/pstore-format/tests/data/generate_lance.py`) and pyarrow-written Arrow IPC files. ⚠️ The
generator names data files after fresh UUIDs, so re-running it replaces the committed datasets
with equivalent ones under new names.

Commands: `cargo test -p pstore-format --test lance`: 12 tests; `cargo test -p pstore-format
--test parquet`: 32 tests; `cargo test -p pstore-format --lib`: 26 tests; `cargo test -p
pstore-index --test external`: 16 tests.

1. `a_multi_fragment_dataset_reads_back_every_row`: the real `many` dataset (three data files,
   2,600 rows) pins as one column of three objects, runs in row order across them, and every row
   equals the generator's floats.
2. `a_deleted_row_is_never_returned`: the real `deleted` dataset (rows 0 to 9 of 300) pins with
   300 rows; those ten are absent and `Absent` to read; the other 290 equal the generator's
   floats. ⚠️ Found by reading the real file: Lance 13's deletion file is **zstd-compressed** Arrow
   IPC, which the first version of the reader refused.
3. `a_changed_fragment_is_noticed` and `any_one_object_changing_is_noticed`: replacing any one data
   file is `Changed` whichever rows are asked for (each of three objects tried).
4. `a_column_over_several_objects_is_checked` and
   `reads_across_objects_come_back_in_the_rows_asked`: runs are checked against their own
   object's length, an object the column lacks and a dictionary outside object 0 are refused, and
   six rows asked out of order across three objects come back right.
5. `a_deletion_file_that_lies_is_refused`, `a_deletion_count_that_disagrees_or_a_file_too_large_is_
   refused`, `only_a_plain_or_zstd_list_of_one_uint32_column_is_a_deletion_file`,
   `a_deletion_file_gives_its_sorted_rows`, `a_damaged_deletion_file_is_refused_at_every_cut` and
   `deleted_rows_are_cut_out_of_a_run`: plain and zstd lists are read; lz4, 64-bit, two-column,
   null-holding, two-batch, stream-format and empty Arrow files are refused; a manifest count of 9
   or 11 against 10 listed rows is refused and 0 (unspecified) is not; a deletion file of 64 MiB
   plus one byte is refused unread and exactly 64 MiB is read; every prefix of the real file is
   refused and no flipped byte reads out of bounds; cutting a run around deleted rows is exact at
   every position.
6. `an_index_over_a_multi_fragment_dataset_finds_the_exact_neighbours`,
   `an_index_over_a_dataset_with_deletions_never_returns_a_deleted_row` and
   `objects_without_a_tag_round_trip_through_the_blob`: build, reopen (blob byte-identical) and
   search equal the exact cosine top 10 over the present rows; a changed data file is refused by
   the reopened index; the further objects survive the `PXI6` blob with and without CAS tags.
7. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants`, `--no-config`, changed lines): the full sweep of the format side was **198
   mutants with 10 missed**, all in the Arrow reader's structural checks (and 1 of 8 on the index
   side, a tag-less further object); a first sweep had also missed the deletion-count and size
   bounds and the extent cap, killed by the tests above. Fixtures for each shape (`arrow/`) and
   tests were added, and **every mutant in the reader was re-run: 57 of 57 caught**; the index
   side re-run: **16 mutants, 15 caught, 1 unviable**. The every-prefix test also found a real
   panic (a file shorter than its trailer), now fixed with a checked subtraction.
   ⚠️ `cargo deny check advisories` still fails on `paste` (via `foyer`), as before.

**Not covered:** Lance 2.1+; a bitmap deletion file; picking the latest manifest; a fragment with
its column in several data files.
