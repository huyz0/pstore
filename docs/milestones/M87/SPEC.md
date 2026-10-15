# M87 — A Lance dataset of several fragments, with deletions (PLANNED, not built)

**Serves:** backlog row 73's remainder, after [M82](../M82/SPEC.md). **Status: planned.** No owner
decision is needed; the design choice is made here.

## Decision

An `ExternalColumn` addresses one object. Rather than change that for every reader, a dataset of
several data files is represented as **one logical column over several objects**: each `Extent`
gains `object: u32`, an index into a short list of keys the column carries (the Parquet case is a
list of one). A row's key is then its extent's object; `read_rows` groups ranges by object and
issues one `get_ranges` each. The codes blob (`PXI6`) stores the key list.

Deleted rows are **kept in the numbering and excluded from the answer**: a fragment's deletion
file (an Arrow IPC list of deleted offsets) is read once at pin time into a sorted set stored in
the codes blob; `search` drops those rows after scoring, so a row number stays a position.

## Acceptance criteria

1. `a_multi_fragment_dataset_reads_back_every_row`: the real pylance `many` dataset (3 files)
   pins and every row equals the generator's floats.
2. `a_deleted_row_is_never_returned`: the real `deleted` dataset (ids 0..9 deleted) pins; no search
   returns them, and every other row is returned as in the undeleted dataset.
3. `a_changed_fragment_is_noticed`: replacing any one data file is `Changed`.
4. `the_codes_blob_round_trips_its_objects_and_deletions` (`PXI6`; a `PXI5` blob is rebuilt).
5. `an_index_over_a_multi_fragment_dataset_finds_the_exact_neighbours`.
6. **Gates:** `./scripts/gates.sh`; mutation sweep of the changed lines misses 0.

**Not covered:** Lance 2.1+; picking the latest manifest (the caller still names one: a LIST is
forbidden on read paths).
