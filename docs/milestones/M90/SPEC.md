# M90 — Lance 2.1 vector columns

**Serves:** backlog row 73's last piece. **Status: built.** No owner decision was needed.

## What is known, and what is not

Lance 2.1 (manifest `DataFile` version 2.1, found in the real `v21` fixture and refused today as
"format 2.1") replaces 2.0's `ArrayEncoding` protobuf with **structural encodings**: a page is
`MiniBlock` or `FullZip`, with repetition/definition levels and optional compression. A
null-free, uncompressed `FixedSizeList<float32>` is expected to be a `FullZip` page whose values
are contiguous, but **that is an expectation read from the format's documentation, not yet from a
real file**. So the first step is to read one.

## Finding (step 1, done)

Read from the real `v21` fixture (pylance 13, `data_storage_version='2.1'`, 300 rows of 8
`float32`): the vector column's page is `PageLayout.full_zip` (type URL
`/lance.encodings21.PageLayout`) with two buffers, at offsets 1280 and 1344 and of 6 and 9,624
bytes. The second is **not** a plain run: the first vector is 8 bytes in (`00000010 fefefefe`
precede it), and **only the first 128 vectors are contiguous at the 32-byte stride**: the rest
sit after further chunk boundaries (9,600 bytes of vectors plus 24 of framing). The 6-byte first
buffer is presumably the chunk index. So a vector page is **chunked** (128 values a chunk, each
chunk led by a header), and a row's byte range needs the chunk index decoded, not a stride.

## Finding (step 2: 130 shapes, one writer)

The first finding was read from one file and so was not enough. This milestone wrote 40-odd
datasets with pylance 13 across rows 1 to 200,000 and dimensions 1 to 16,384, and read every
vector's position back from the bytes. Two layouts hold a null-free `FixedSizeList<float32>`:

- **`FullZip`** (rows of 256 bytes or more, dimensions 64 and up): one buffer, the rows back to
  back with no framing. The layout's `bits_per_value` is `dims × 32`.
- **`MiniBlock`** (smaller rows): two buffers. The first is a `u16` a chunk, `words << 4 |
  log2(values)`, where `words` is the chunk's rows padded to 8 bytes, in 8-byte words, and the
  log is 0 for a last chunk that is not full. The second is the chunks, each an **8-byte
  header** (two `u16`s and padding) and its rows padded to 8 bytes. **Every chunk's offset is
  arithmetic over the first buffer**, so one 2-byte-a-chunk read at build time pins them all; a
  query reads nothing new. (The writer cuts chunks at about 4,096 bytes, rounded up to a power of
  two of rows; that is a heuristic the reader does not rely on.)
- Nulls change the layout (a `layers` of 3, two buffers, repetition and definition
  compression): refused.

## What is read, and refused

Read: a page layout whose protobuf is exactly the shape above, `FixedSizeList(dims)` over a flat
32-bit value with no buffer compression, a `layers` of `[1]`, and chunk sizes that account for the
page's rows and its buffer to the byte. Refused, by name: anything else in a layout, a chunk that
declares no size, a chunk of no rows, sizes that disagree with the page, a buffer past the file.
`locate` accepts manifest version 2.1 with a 2.1 footer (2.1, where 2.0 is 0.3); each version's
files must match their own footer.

⚠️ **A behaviour change recorded here, not a weakened test:** `v21` was refused as "format 2.1"
by `a_column_that_cannot_be_read_in_place_is_refused_by_name`, and the synthetic file-version
case asserted the same. The first is now a reading test; the second asserts the footer check.

## Acceptance criteria

1. `a_lance_2_1_dataset_reads_back_every_row`: real datasets of 1, 129, 256, 2,600 rows of 3 or 8
   floats (`MiniBlock`, one chunk, a padded chunk, an exact fit, 21 chunks) and 300 rows of 100
   (`FullZip`) read back every row exactly.
2. `a_2_1_column_with_nulls_is_refused_by_name`.
3. `a_damaged_chunk_index_is_refused_by_name`,
   `a_page_whose_chunks_do_not_fill_its_buffer_is_refused`,
   `a_layout_that_is_not_a_plain_null_free_run_is_refused_by_name` and
   `a_full_zip_page_must_be_exactly_its_rows`: byte-patched copies of the real files, each
   refused with its reason.
4. **Gates:** `./scripts/gates.sh`; the mutation sweep of the changed lines misses 0.

**Not covered:** nullable columns; compressed values; `list<float>`-style (variable) vectors;
a page of more than 65,535 chunk-bytes of header fields (a writer's `has_large_chunk`); other
writers than pylance 13.

**Size:** M.
