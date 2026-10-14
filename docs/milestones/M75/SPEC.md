# M75 — Snappy-compressed Parquet columns

**Serves:** backlog row 74. [M70](../M70/SPEC.md) refused every compressed column. Snappy is
Parquet's default, so most real files were refused.

## Cost, stated first

A compressed page has no computable per-row range: a candidate row costs its **whole page** (a
ranged read of the page's compressed bytes) plus a decompress. The build reads every page once
anyway. The search pays this only in the rerank, which is already the opt-in round outside the
three-round budget (D-34). A page is small (Parquet's default is 1 MB), and rows of one page that
are candidates share one read.

## Delta

- `Extent` gains `compressed: u64`: 0 means raw (M70's extents); otherwise the page's compressed
  byte length at `offset`, **snappy**.
- `parquet::locate` accepts codec `SNAPPY` as well as `UNCOMPRESSED` (still `REQUIRED`,
  `FIXED_LEN_BYTE_ARRAY`, `PLAIN`, no dictionary, v1 pages). Every other codec is refused naming it.
  A snappy page's decompressed size must equal `values × width`.
- `ExternalColumn::read_rows` reads a compressed extent's whole page once, decompresses it, and
  slices the rows; raw extents are read as before. `row_range` is `None` for a compressed row
  (there is no per-row range).
- The index blob (`PXI3`) carries `compressed` per extent.

## Acceptance criteria

1. `a_snappy_file_locates_and_reads_back` (`pstore-format`): the pyarrow-written
   `multipage-snappy.parquet` (2,600 rows, several snappy pages) locates and every row reads back
   exactly the floats the generator wrote; `snappy.parquet` (3 row groups) likewise.
2. `other_codecs_are_refused_by_name`: `zstd.parquet` is refused naming the codec.
3. `a_snappy_page_is_read_once_for_all_its_candidates`: reading two rows of one page moves that
   page's compressed bytes once; rows on two pages move two pages and no others.
4. `a_damaged_snappy_page_is_an_error`: a page whose bytes are corrupted, or whose decompressed
   length is not `values × width`, is an error, never wrong floats.
5. `an_index_over_each_codec_finds_the_exact_neighbours` (`pstore-index`): build, reopen and
   search `multipage-snappy.parquet` at a full window equals the exact cosine top 10.
6. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the new code misses 0, or each
   miss is killed by a named test.

**Not covered:** zstd, gzip, lz4 and brotli; dictionary encoding; v2 pages.
