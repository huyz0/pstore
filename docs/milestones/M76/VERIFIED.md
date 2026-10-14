# M76 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store, real pyarrow-written files.
Compression ratios are not claimed.

Commands: `cargo test -p pstore-format --test parquet`: 15 tests; `cargo test -p pstore-index
--test external`: 11 tests.

1. `gzip_and_zstd_files_read_back_every_row`: `multipage-gzip.parquet` and
   `multipage-zstd.parquet` (2,600 rows, several pages) locate with codec 2 and 6 on every extent
   and every row equals the generator's floats; `zstd.parquet` (3 row groups) likewise.
2. `lzo_and_hadoop_lz4_are_refused_by_name` (named `brotli_and_other_codecs_are_refused_by_name` when M76 shipped; renamed by M79, which made brotli readable): `brotli.parquet` and synthetic codecs 4, 5 and 7
   are refused naming "only snappy, gzip and zstd".
3. `a_page_that_lies_about_its_size_cannot_balloon`: a 64 MB page of zeros, gzip- and zstd-coded
   (under 1 MB on disk), in an extent claiming 32 bytes, is `Damaged`; the same codecs told the
   truth read fine; an unknown codec number is `Damaged`.
4. `a_damaged_page_is_an_error_per_codec`: 64 scribbled bytes in a gzip page and in a zstd page are
   each `Damaged`.
5. `an_index_over_each_codec_finds_the_exact_neighbours`: snappy, gzip and zstd 2,600-row files each
   build, reopen (blob byte-identical) and search to the exact cosine top 10 at a full window; a
   changed object is refused.
6. **Gates:** green by the pre-commit hook on this commit. **Mutation** (`cargo mutants`,
   `--no-config`, changed lines): format side **21 mutants, 20 caught, 1 unviable, 0 missed**;
   index side 4 mutants, 3 caught, 1 unviable, 0 missed.
   ⚠️ `cargo deny check advisories` still fails on `paste` (via `foyer`), as before this change.

**Not covered:** brotli, lz4, lzo; dictionary encoding; v2 pages. ⚠️ A `PXI3` codes blob (M75's) is
read as not an external index and must be rebuilt; none exists outside this branch.
