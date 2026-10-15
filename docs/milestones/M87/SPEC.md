# M87 — A Lance dataset of several fragments, with deletions

**Serves:** backlog row 73's remainder, after [M82](../M82/SPEC.md) and [M88](../M88/SPEC.md).

## Decision

An `ExternalColumn` addressed one object. A dataset of several data files is **one logical column
over several objects**: each `Extent` gains `object: u32`, an index into the column's own object
(0) and `ExternalColumn::more` (1..). `read_rows` groups the ranges it needs by object and asks
each object once, concurrently, returning rows in the order asked; it checks **every** object's
length and tag first, so a change to any one is `Changed`. The Parquet case is a list of one.

**Deleted rows are gaps**, exactly as [M88](../M88/SPEC.md)'s nulls are: a fragment's deletion
file is read once at pin time and the rows it lists are cut out of that fragment's runs, so a row
keeps its number and has no vector. No separate deleted-set is stored or consulted at search.

- `lance::locate` returns a `Dataset` (the data files and the runs); every fragment must hold the
  column in exactly one data file; a fragment's rows follow the previous fragment's.
- The deletion file is an Arrow IPC *file* of one non-null `uint32` column, **zstd-compressed** by
  Lance 13 (found by reading a real one, not the documentation). It is read by a small
  FlatBuffers reader for the footer, message and record batch: untrusted, every offset checked,
  decompression bounded by the row count, other compression, several batches or columns, nulls,
  another width and rows at or past the fragment's rows are refused. A bitmap deletion file is
  refused by name.
- The codes blob is `PXI6`: each extent names its object and the further objects follow the
  extents. (A `PXI5` blob is rebuilt.)
- ⚠️ Bounded: a column splits into at most 2^22 runs, a deletion file is at most 64 MiB, and a
  fragment's deleted rows decode to at most 256 MiB.

## Acceptance criteria

1. `a_multi_fragment_dataset_reads_back_every_row`: the real pylance `many` dataset (three data
   files, 2,600 rows) pins with the column spanning all three objects in row order; every row
   equals the generator's floats.
2. `a_deleted_row_is_never_returned`: the real `deleted` dataset (rows 0 to 9 of 300 deleted)
   pins; the deleted rows are absent, and asking for one is `Absent`; the other 290 equal the
   generator's floats.
3. `a_changed_fragment_is_noticed` and `any_one_object_changing_is_noticed`: replacing any one
   data file is `Changed`, whichever rows are asked for.
4. `a_column_over_several_objects_is_checked` and `reads_across_objects_come_back_in_the_rows_
   asked`: runs are checked against their own object's length, a missing object and a dictionary
   in a later object are refused, and rows asked out of order across three objects come back right.
5. `a_deletion_file_that_lies_is_refused`, `a_deletion_file_gives_its_sorted_rows`,
   `a_damaged_deletion_file_is_refused_at_every_cut` and `deleted_rows_are_cut_out_of_a_run`: a
   truncated, empty, mislabelled and mis-sized deletion file is refused by name; every prefix of
   the real one is refused and no flipped byte reads out of bounds; the cutting of a run around
   deleted rows is exact at every position.
6. `an_index_over_a_multi_fragment_dataset_finds_the_exact_neighbours` and
   `an_index_over_a_dataset_with_deletions_never_returns_a_deleted_row`: build, reopen (blob
   byte-identical) and search equal the exact cosine top 10 over the present rows; a changed data
   file is refused by the reopened index.
7. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0.

**Not covered:** Lance 2.1+; a bitmap deletion file; picking the latest manifest (the caller names
one: a LIST is forbidden on read paths); a fragment with its column in several data files.
