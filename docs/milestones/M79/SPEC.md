# M79 — Brotli and LZ4 Parquet pages

**Serves:** backlog row 74's remainder, after [M75](../M75/SPEC.md) (snappy) and
[M76](../M76/SPEC.md) (gzip, zstd).

## Delta

- `locate` accepts Parquet codecs 4 (brotli) and 7 (`LZ4_RAW`, one lz4 block); the extent's
  `codec` byte and the bounded decompression of M76 cover them: brotli is read at most
  `expected + 1` bytes, and the lz4 block decoder is given `expected` as its output bound.
- ⚠️ **pyarrow writes both its `LZ4` and `LZ4_RAW` spellings as codec 7**; the two fixtures were
  byte-identical, so one is kept. The deprecated Hadoop-framed `LZ4` (5) and LZO (3) are refused
  naming the codecs read.

## Acceptance criteria

1. `gzip_and_zstd_files_read_back_every_row` (now also brotli and lz4): pyarrow-written
   `multipage-brotli.parquet` and `multipage-lz4raw.parquet` (2,600 rows, several pages) locate with
   codec 4 and 7 and every row equals the generator's floats; `brotli.parquet` (3 row groups)
   likewise.
2. `lzo_and_hadoop_lz4_are_refused_by_name`: codecs 3 and 5 are refused naming "only snappy, gzip,
   brotli, lz4_raw and zstd".
3. `a_page_that_lies_about_its_size_cannot_balloon`: brotli and lz4 pages of 64 MB of zeros (under
   1 MB on disk) in an extent claiming 32 bytes are `Damaged`; told the truth they read.
4. `a_damaged_page_is_an_error_per_codec`: corrupted brotli and lz4 pages are `Damaged`.
5. `an_index_over_each_codec_finds_the_exact_neighbours`: brotli and lz4 files join snappy, gzip and
   zstd: each builds, reopens and searches to the exact cosine top 10 at a full window.
6. **Gates:** `./scripts/gates.sh` passes; `cargo deny check licenses bans sources` passes; the
   mutation sweep of the changed lines misses 0.

**Not covered:** dictionary encoding; v2 pages; Hadoop-framed lz4 and lzo.
