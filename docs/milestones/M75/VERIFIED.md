# M75 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store. The Parquet fixtures are
real files written by pyarrow 26 (`generate.py` regenerates them; the earlier fixtures came back
byte-identical). ⚠️ The random floats barely compress (a page is 32,774 bytes against 32,768), so
the codec path is exercised but **no compression ratio is claimed**.

Commands: `cargo test -p pstore-format --test parquet`: 11 tests; `cargo test -p pstore-index
--test external`: 11 tests.

1. `a_snappy_file_locates_and_reads_back`: `snappy.parquet` (3 row groups) and
   `multipage-snappy.parquet` (2,600 rows, several snappy pages) read back every row equal to the
   generator's floats; every extent is compressed; a compressed row has no per-row range.
2. `locate_refuses_what_cannot_be_reached`: `zstd.parquet` is refused naming "compressed with
   codec"; `locate_refuses_each_shape_it_cannot_read`: codecs 2 and 6 are refused naming "only
   snappy".
3. `a_snappy_page_is_read_once_for_all_its_candidates`: two rows of one page move exactly that
   page's 32,774 compressed bytes; rows on two adjacent pages move both and at most 64 bytes more
   (the page header the coalescer bridges).
4. `a_damaged_snappy_page_is_an_error`: 40 scribbled bytes in a page, and a page that decompresses
   cleanly to 8 bytes where 32 are due, are each `Damaged`, never floats.
   `snappy_pages_are_checked_against_their_claims` and
   `a_compressed_extent_is_bounded_by_its_compressed_size`: a compressed size of 0, a decompressed
   size 4 over or under, and a page longer than its chunk are refused; a compressed extent is
   bounded by its compressed bytes, to the byte.
5. `an_index_over_each_codec_finds_the_exact_neighbours` (renamed by M76, which added gzip and zstd to it; the snappy file is its first case): build, reopen (blob byte-identical)
   and search `multipage-snappy.parquet`; at a full window the top 10 for three queries equals the
   exact cosine; a changed object is refused.
6. **Gates:** green by the pre-commit hook on this commit. **Mutation** (`cargo mutants`,
   `--no-config`, changed lines of `parquet.rs`, `external.rs` and the index's `external.rs`):
   first sweep 68 + 4 mutants, 3 missed; each is killed by the tests in criterion 4 (re-run on
   those lines: **13 mutants, 13 caught**); index side 4 mutants, 3 caught, 1 unviable.
   ⚠️ `cargo deny check` fails on a RUSTSEC unmaintained advisory for `paste` (via `foyer`); it
   fails identically on the tree before this change.

**Not covered:** zstd, gzip, lz4, brotli; dictionary encoding; v2 pages. ⚠️ A `PXI2` codes blob
(M70's) is read as not an external index and must be rebuilt; none exists outside this branch.
