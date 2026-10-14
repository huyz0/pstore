# M81 — Dictionary-encoded Parquet vector columns

**Serves:** backlog row 74's last codec-side item, after [M75](../M75/SPEC.md)–[M80](../M80/SPEC.md).
`use_dictionary=True` is pyarrow's default, and a column of few distinct vectors (an embedding of a
category, a quantised table) keeps its dictionary.

## What it is, and what it costs

A dictionary-encoded chunk is a **dictionary page** (a plain run of the distinct vectors) followed
by pages of **indices**, Parquet's RLE / bit-packed hybrid: one byte of bit width, then runs. A
row's vector is `dictionary[index]`, and an index is reachable only by decoding its page from the
start, so a candidate row costs its **whole index page and the chunk's dictionary**, read once
each per call and decoded. Like the compressed case, that is acceptable at build time (every page
is read anyway) and in the opt-in rerank.

## Delta

- `Extent` gains `plain_len` (the decoded index page's size) and `dict: Option<DictRef>`
  (`offset`, `compressed`, `count`, `codec`): an extent with `dict` is a page of indices. A
  dictionary-encoded page has `compressed > 0` whatever its codec: it has no per-row range.
- `locate` starts a chunk at its `dictionary_page_offset` when it has one, reads the dictionary
  page (plain, `count × width` bytes decoded, compressed with the chunk codec), and emits an extent
  per `PLAIN_DICTIONARY` / `RLE_DICTIONARY` page. **A chunk may mix** dictionary pages and plain
  ones (a writer falls back to plain when its dictionary grows too large).
- ⚠️ **Refused:** a chunk that declares a dictionary page but does not begin with one; a
  dictionary page anywhere else or twice; dictionary-encoded indices before any dictionary; a
  dictionary or index page claiming more than 256 MiB decoded; a dictionary whose size is not
  `count × width`.
- `read_rows` decodes with `decode_indices`, which returns exactly `rows` indices all below
  `count` or nothing: a run of zero values (which would never advance), a header that does not end,
  a run cut short, or an index outside the dictionary is `Damaged`.
- The codes blob is `PXI5`.

## Acceptance criteria

1. `dictionary_files_read_back_every_row`: pyarrow-written `dictionary.parquet`,
   `dictionary-snappy.parquet` and `dictionary-v2.parquet` (2,600 rows over 40 distinct vectors,
   several index pages) locate with one shared dictionary of 40, every row equals the generator's
   floats; the snappy file's pages and dictionary are snappy. `dict.parquet` (100 distinct a row
   group, three groups) reads back too.
2. `rle_and_bit_packed_runs_decode_and_mix` and
   `a_body_that_is_not_n_indices_below_count_is_refused` (unit): RLE, bit-packed (3 and 17 bits),
   mixed runs, a multi-byte run header and width 0 decode; each malformed body above is refused.
3. `a_dictionary_is_read_once_for_all_its_candidates`: two rows of one page move that page and the
   dictionary once; rows on two pages move both pages and still one dictionary.
4. `a_damaged_dictionary_page_is_an_error`: a scribbled index page, a dictionary claiming one
   vector too many, and extents with a dictionary that is empty, past the object, overflowing, or
   with no page body are each an error.
5. `locate_refuses_each_shape_it_cannot_read`: a declared dictionary that does not come first, a
   dictionary page in a chunk that declared none, and indices before any dictionary are refused by
   name.
6. `an_index_over_each_codec_finds_the_exact_neighbours`: the three dictionary files join the other
   codecs: each builds, reopens and searches to the exact cosine top 10 at a full window.
7. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0.

**Not covered:** page checksums are not verified (a scribbled dictionary changes floats
undetectably); nullable and nested columns; a dictionary larger than 256 MiB.
