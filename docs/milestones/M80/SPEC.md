# M80 — Parquet v2 data pages

**Serves:** backlog row 74's remainder, after [M75](../M75/SPEC.md), [M76](../M76/SPEC.md) and
[M79](../M79/SPEC.md). `data_page_version="2.0"` is how newer writers lay out a column.

## Delta

`locate` reads a v2 data page header (`DataPageHeaderV2`) as well as a v1's. A v2 page's
repetition and definition levels sit *outside* the compressed values; a `REQUIRED` flat column has
none, so the page is its values alone. It is accepted only with **no nulls and zero-length levels**
(otherwise `levels or nulls`, refused). Whether the values are compressed is the **page's**
`is_compressed`, not only the chunk's codec: a page marked uncompressed is raw even in a snappy
chunk. ⚠️ This is not hypothetical: pyarrow writes a v2 page compressed only when that makes it
smaller, and random floats barely shrink under snappy, so its v2 snappy file holds raw pages.

## Acceptance criteria

1. `v2_files_read_back_every_row`: pyarrow-written `multipage-v2.parquet` (no codec),
   `multipage-v2-snappy.parquet` (pages left raw by the writer) and `multipage-v2-zstd.parquet`
   (pages compressed) each locate, every extent is raw or fully compressed (never half), and every
   row equals the generator's floats. **Seen red** against the previous parser.
2. `a_v2_page_is_checked_against_its_claims`: nulls, a definition level, a repetition level are
   each refused; a page marked uncompressed in a snappy chunk reads as raw; a compressed one is
   sized by its on-disk bytes; a v2 header on a v1 page type, and a v1 header on a v2 page type,
   are refused. **Seen red** against the previous parser.
3. `an_index_over_each_codec_finds_the_exact_neighbours`: the three v2 files join the other codecs:
   each builds, reopens and searches to the exact cosine top 10 at a full window.
4. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0.

**Not covered:** dictionary encoding (a dictionary page plus RLE-bit-packed indices); nullable and
nested columns.
