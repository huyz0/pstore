# M82 — Lance datasets as an external column

**Serves:** backlog row 73 (OQ-169), after the Parquet reader of [M70](../M70/SPEC.md)–[M81](../M81/SPEC.md).

## What is read, and what is refused

A Lance 2.0 dataset names its data files in a **manifest** (protobuf, `_versions/<n>.manifest`); a
data file ends in a 40-byte footer that leads to each column's page table (protobuf). A float32
fixed-size-list column with no nulls is stored as raw little-endian floats, one buffer a page, so
each page is one contiguous extent, exactly as an uncompressed Parquet page is. `ExternalColumn::
pin_lance(store, manifest_key, column)` binds the column to its **data file's** key; everything
downstream (index build, search, rerank) is the code M68–M81 already tested.

Each shape this does not read in place is refused **with the reason**:

| Shape | Why |
|---|---|
| more than one fragment, or the column in more than one data file | an `ExternalColumn` addresses one object |
| a fragment with a deletion file | a row number would no longer be a position |
| manifest data-file format other than 2.0 (e.g. 2.1), or footer other than 0.3 | other encodings |
| a column that is not `fixed_size_list:float:N`, or whose page encoding is not `Nullable(NoNull(FixedSizeList(N, Nullable(NoNull(Flat(32 bits))))))` | nulls, other element types, compression |
| a page of more than one buffer, a page whose size is not `rows × N × 4`, a page past the file, pages not summing to the fragment's rows, a data file whose size disagrees with the manifest | a lying page table |
| a manifest not under `_versions/`, a data path with `..` or a leading `/` | a path that escapes the dataset |

⚠️ The manifest and page tables are untrusted: manifest and column metadata are capped at 8 MiB,
every offset is checked against the object, and the protobuf reader returns `None` for anything it
does not understand.

⚠️ **Format facts found by reading real pylance 13 output, not the documentation:** the manifest's
`DataFile` version for "Lance 2.0" is `2.0`, while the data file's own footer says `0.3`; the
manifest is length-prefixed and the footer's position points at the prefix; the top-level column
encoding is wrapped in `Nullable(NoNull(...))` as its items are.

## Acceptance criteria

1. `a_lance_dataset_reads_back_every_row`: a pylance-written dataset of 2,600 8-dimensional rows
   pins, is bound to its data file, and every row equals the generator's floats; a row costs
   32 bytes.
2. `a_column_that_cannot_be_read_in_place_is_refused_by_name`: the real `many` (3 fragments),
   `deleted`, `nullable` and `v21` datasets are each refused with their reason; a column the
   dataset lacks and a non-vector column are refused.
3. `several_pages_are_one_extent_each`: a synthetic data file of three pages (3, 4 and 5 rows)
   yields three extents in order and every row reads back. (pylance writes one page at this size,
   so the multi-page path has no real-file fixture; the builder writes the same protobuf.)
4. `a_page_table_that_lies_is_refused`: each way the page table can disagree with itself or the
   file (sizes, offsets, buffers, bits, dimension, type URL, row counts) is refused by name.
5. `a_manifest_or_file_that_is_not_lance_is_refused`: bad magic, footer and manifest versions, a
   wrong data-file size, escaping paths, other logical types, truncated and absent objects.
6. `a_changed_data_file_is_noticed`: a data file replaced after pinning is `Changed`.
7. `an_index_over_a_lance_dataset_finds_the_exact_neighbours` (`pstore-index`): build, reopen
   (blob byte-identical) and search at a full window equals the exact cosine top 10 for three
   queries; a changed data file is refused.
8. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the new file and `pin_lance`
   misses 0, or each miss is killed by a named test.

**Not covered:** several fragments or data files; deleted rows; Lance 2.1+; compressed or nullable
vector columns; choosing "the latest" manifest (the caller names one); a dataset whose `data/`
lives outside the manifest's directory tree.
