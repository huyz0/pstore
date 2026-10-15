# M84 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, real pyarrow-written files.

Commands: `cargo test -p pstore-format --test parquet`: 26 tests; `cargo test -p pstore-format
--lib`: 20 tests; `cargo test -p pstore-index --test external`: 12 tests.

1. `optional_files_without_nulls_read_back_every_row`: `optional.parquet` (three row groups),
   `optional-multipage.parquet` (v1), `optional-v2.parquet` and `optional-v2-zstd.parquet` (2,600
   rows, several pages) each locate and every row equals the generator's floats.
2. `an_optional_column_that_cannot_be_read_in_place_is_refused_by_name`: `optional-nulls.parquet`
   (every tenth value null) is "holds nulls"; `optional-snappy.parquet` names the compressed body;
   `optional-dictionary.parquet` is refused as a dictionary-encoded optional column.
3. `damaged_definition_levels_are_refused`: a level byte flipped to null, a length that runs one
   byte past the page, a huge one and a zero one are refused by name; levels running exactly to
   the page's end are not refused for running past it.
4. `levels_give_the_runs_of_present_rows_exactly` (renamed by M88, which turned the all-present check into present runs): the cases in the spec, including the six-byte
   run header and a packed run cut by `n`.
5. `locate_refuses_each_shape_it_cannot_read` and
   `an_optional_page_is_bounded_before_its_levels_are_read`: `REPEATED` is refused; an OPTIONAL
   schema over pages with no levels is refused; a page of no values runs past its chunk; 2^24
   four-wide vectors (256 MiB) are read on to their levels and one more is "too many values".
   ⚠️ `optional.parquet` moved from the refusals to criterion 1: a deliberate change of
   behaviour, recorded in the spec.
6. `an_index_over_each_codec_finds_the_exact_neighbours`: `optional-multipage`, `optional-v2` and
   `optional-v2-zstd` join the other files and match the exact cosine top 10 at a full window.
7. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants`, `--no-config`, changed lines of `parquet.rs`): two sweeps found misses (a
   redundant guard, header-length and `n`-cut boundaries, an equivalent subtraction, the
   dictionary and size bounds); the redundant guard was removed, the equivalent arithmetic
   rewritten, and tests added for the rest. Final sweep: **110 mutants, 109 caught, 1 unviable,
   0 missed.** ⚠️ `cargo deny check advisories` still fails on `paste` (via `foyer`), as before.

**Not covered:** nulls (a row with no vector; read by [M88](../M88/SPEC.md), which replaced this milestone's "holds nulls" refusal); dictionary-encoded or compressed-v1 optional
columns; nested columns; maximum definition levels above 1.
