# M88 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, real pyarrow-written files.

Commands: `cargo test -p pstore-format --test parquet`: 29 tests; `cargo test -p pstore-format
--lib`: 22 tests; `cargo test -p pstore-index --test external`: 13 tests.

1. `a_column_with_nulls_reads_back_every_present_row`: the real `optional-nulls.parquet` (2,600
   rows, every tenth null, several pages) pins with 2,600 rows; `is_present` and `row_range` are
   true for exactly the present rows; all 2,340 present rows equal the generator's floats; reading
   null rows 3, 13 and 2,593 is `Absent`; the runs have gaps. `v2_pages_with_nulls_agree_with_
   their_headers` does the same for `optional-nulls-v2.parquet` (raw v2 pages, header null
   counts equal to the levels'), and refuses `optional-nulls-v2-zstd.parquet` as a compressed
   page holding nulls.
2. `runs_with_gaps_are_checked`: gaps and a trailing gap are accepted; out-of-order, overlapping,
   past-the-column and empty runs are refused; `from_extents` still refuses gaps; a strided
   column has no row at its `rows`.
3. `levels_give_the_runs_of_present_rows_exactly`, `the_extent_cap_is_exact` and
   `a_page_of_alternating_rows_is_refused_past_the_run_cap`: the runs for RLE, bit-packed and
   mixed levels, leading and trailing nulls, runs cut at the page's end wherever they begin,
   padding bits; 2^20 runs a page is allowed and 2^20 + 1 is not; 2^22 extents a column likewise.
4. `an_index_over_a_column_with_nulls_finds_the_exact_neighbours`: build, reopen (blob
   byte-identical) and search equal the exact cosine top 10 over the present rows for five
   queries (two of them null rows' own vectors); no null row is returned, with or without a
   filter.
5. `damaged_definition_levels_are_refused` and
   `an_optional_column_that_cannot_be_read_in_place_is_refused_by_name`: a level flipped to null is
   refused by name ("carries levels", the page's size no longer matching); compressed v1 and
   dictionary-encoded optional columns still are.
6. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants`, `--no-config`, changed lines): format side, after two rounds of added tests
   and removing a redundant guard: **147 mutants, 133 caught, 14 unviable, 0 missed.** Index side:
   16 mutants, 13 caught, 2 unviable, 1 timeout (`(at + chunk)` becoming a product, which never
   advances: a hang, and so caught). ⚠️ `cargo deny check advisories` still fails on `paste`
   (via `foyer`), as before.

**Not covered:** nested columns; a maximum definition level above 1; nulls in a compressed page.
