# M76 — Gzip and zstd Parquet pages

**Serves:** backlog row 74's remainder, after [M75](../M75/SPEC.md) (snappy). Gzip and zstd are the
other codecs pyarrow and Spark write by default for lake data.

## Delta

- `Extent` gains `codec: u8` (Parquet's number: 1 snappy, 2 gzip, 6 zstd; 0 raw).
- `locate` accepts codecs 1, 2 and 6; any other (brotli, lz4, lzo) is refused naming the codec and
  the three that are read.
- `ExternalColumn::read_rows` decompresses by the extent's codec to **exactly** `rows × width`
  bytes. ⚠️ **Bounded while decoding:** gzip and zstd read at most `expected + 1` bytes, and snappy's
  own declared length is checked before it allocates, so a page that lies about its size cannot
  balloon memory; any length but the expected one is `Damaged`.
- The codes blob is `PXI4` (a codec byte per extent).

## Acceptance criteria

1. `gzip_and_zstd_files_read_back_every_row`: pyarrow-written `multipage-gzip.parquet` and
   `multipage-zstd.parquet` (2,600 rows, several pages each) locate with the right codec, and every
   row equals the generator's floats; `zstd.parquet` (3 row groups) likewise.
2. `brotli_and_other_codecs_are_refused_by_name`: `brotli.parquet` and synthetic codecs 4, 5 and 7
   are refused naming "only snappy, gzip and zstd".
3. `a_page_that_lies_about_its_size_cannot_balloon`: a gzip page and a zstd page of 64 MB of zeros
   (tiny on disk), in an extent that claims 32 bytes, are `Damaged`, and the read allocates well
   under the lie (asserted by the decoder's bound, not by measurement of the host).
4. `a_damaged_page_is_an_error_per_codec`: corrupted gzip and zstd pages are `Damaged`.
5. `an_index_over_each_codec_finds_the_exact_neighbours` (`pstore-index`): build, reopen and
   search each of the snappy, gzip and zstd 2,600-row files at a full window equals the exact
   cosine top 10.
6. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0, or
   each miss is killed by a named test.

**Not covered:** brotli, lz4, lzo; dictionary encoding; v2 pages.
