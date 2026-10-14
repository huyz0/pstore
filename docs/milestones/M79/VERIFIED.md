# M79 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, real pyarrow-written files.
No compression ratio is claimed. The fixture generator is `crates/pstore-format/tests/data/generate.py`;
the earlier fixtures came back byte-identical.

Commands: `cargo test -p pstore-format --test parquet`: 15 tests; `cargo test -p pstore-index
--test external`: 11 tests.

1. `gzip_and_zstd_files_read_back_every_row`: `multipage-brotli.parquet` and
   `multipage-lz4raw.parquet` (2,600 rows, several pages) locate with codec 4 and 7 on every extent
   and every row equals the generator's floats; `brotli.parquet` (3 row groups) likewise.
2. `lzo_and_hadoop_lz4_are_refused_by_name`: synthetic codecs 3 and 5 are refused naming "only
   snappy, gzip, brotli, lz4_raw and zstd".
3. `a_page_that_lies_about_its_size_cannot_balloon`: brotli and lz4 pages that inflate to 64 MB of
   zeros (each under 1 MB on disk), in an extent claiming 32 bytes, are `Damaged`; the same codecs
   told the truth read.
4. `a_damaged_page_is_an_error_per_codec`: 64 scribbled bytes in a brotli page and in an lz4 page
   are each `Damaged`.
5. `an_index_over_each_codec_finds_the_exact_neighbours`: snappy, gzip, zstd, brotli and lz4 files
   each build, reopen and search to the exact cosine top 10 at a full window; a changed object is
   refused.
6. **Gates:** `./scripts/gates.sh`, green by the pre-commit hook on this commit;
   `cargo deny check licenses bans sources` passes with the brotli and lz4 crates.
   **Mutation** (`cargo mutants`, `--no-config`, changed lines of `parquet.rs` and `external.rs`):
   **6 mutants, 5 caught, 1 unviable, 0 missed.** ⚠️ `cargo deny check advisories` still fails on
   `paste` (via `foyer`), as before this change.

**Not covered:** dictionary encoding; v2 pages; Hadoop-framed lz4 and lzo. A `PXI4` codes blob
needs no rebuild: the extent layout did not change.
