# M90 — Lance 2.1 vector columns (PLANNED; step 1 done, finding below)

**Serves:** backlog row 73's last piece. **Status: planned.** No owner decision is needed.

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

## Revised plan

Step 2 stands only if the chunk header and index layouts can be pinned from more than one real
file (this repo has one and the format's reference is not in it). Until then Lance 2.1 stays
refused by name, which is the honest answer for a format read from a single sample.

## Plan

1. Write a throwaway dumper over the `v21` fixture's page table to see the real protobuf (as M82
   did for 2.0, which found three facts the documentation did not give).
2. If the vector column is a plain contiguous run: read it as M82 does, with a `2.1` arm in the
   version check and the new page-encoding check; any other layout (`MiniBlock`, compressed
   `FullZip`, nullable) stays refused by name.
3. If it is not (for example vectors are bit-packed or chunked), say so in this spec and stop:
   an honest "Lance 2.1 vector columns cannot be read in place" is a result.

## Acceptance criteria

1. The `v21` fixture's page table is dumped and recorded in this spec (the finding).
2. If readable: `a_lance_2_1_dataset_reads_back_every_row` (the fixture, every row equal to the
   generator's floats) and `a_2_1_page_of_another_layout_is_refused_by_name`.
3. **Gates:** `./scripts/gates.sh`; the mutation sweep of the changed lines misses 0.

**Size:** M if readable, S if not.
