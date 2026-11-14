# M233: late materialization of columnar blocks

## Serves
Item 4 of [the column-storage adoption list](../../comparisons/column-storage-adoption.md).
Extends M119's projections (a mask builds only what its predicate reads) to the two scans
that still build every row whole. D-34 unchanged.

## Delta
Today an aggregation (`pstore_query::aggregate`) and an order-by (`select`, and the scoring
expressions' `visit_opened`) decode every kept block with `Projection::ALL`: every row's id
and every attribute, built as owned values, then test the filter and drop most of them.

After, a block is decoded in **two passes**:
1. **First**, only what deciding needs: the filter's attributes, the id when a shadowed
   segment or the filter reads it, and for an order-by its keys' attributes and the id.
2. **Admit**: deleted, shadowed and filtered-out rows are dropped; for an order-by, a row the
   segment's selector, full, would reject at once is dropped too.
3. **Then**, only for the admitted rows: what the consumer reads. An aggregation reads its
   aggregates' and group keys' attributes, so no other column is built at all; an order-by
   returns whole rows, so the rest of each admitted row is built.

Columnar blocks (M231) skip a column's values for a row not admitted; a row block decodes as
before, with the two passes' projections joined, so a row block's aggregation still builds
only the attributes it reads. Nothing changes on disk, in requests or in depth.

**No setting**, by the non-negotiable's exception, if criterion 3 holds: the change is CPU
only, never a byte or a request, and is adopted only if no measured case slows by more than
10%. If one does, it is not adopted and the measured cost is recorded.

## Acceptance criteria
1. Equal answers: over seeded segments with deletes and rows held unfolded, written columnar
   and as rows, every aggregate (grouped by value and ungrouped), an order-by of two keys with
   offset and limit, and a scoring expression answer the same under 200 random filters (every
   op on ints, floats and strings, under `And`, `Or`, `Not`, and on the id).
2. Not built: a columnar block whose string column holds bytes that are not UTF-8 in a row the
   filter does not admit decodes without error late, and is refused when that row is admitted
   or decoded whole: an unadmitted row's value is never built.
3. `./scripts/columns.sh` (`provisional`), M232 against M233, columnar blocks without
   aggregate statistics: for each aggregation and three order-bys (unfiltered, a selective
   filter, a filter admitting every row), GETs, bytes and median time. Adopted only if bytes
   and GETs are identical and no case's time rises by more than 10%.

## Test plan
- 1: `late_materialization_answers_as_whole_rows_do` (engine `tests/it/late.rs`).
  Kills: the admit test applied to the first pass's id-less row in a shadowed segment; the
  then-pass dropping an attribute the first pass decoded; a row admitted twice; the selector's
  early rejection using `<=` for `<`.
- 2: `an_unadmitted_row_is_never_built` (pstore-format `columnar.rs` unit test).
- 3: the script.

## RA budget
Unchanged: the same blocks, fetched in the same one coalesced plan per segment.

## Risks
- A filter admitting nearly every row reads each block's header twice and merges two passes;
  the bench's admitting-every-row cases show the cost.
- An order-by over few columns gains little: every admitted row is still built whole.

## Tasks
- M233.1: `columnar::decode_block_late`, `Segment::visit_blocks_late`, the aggregate and
  order-by call sites, tests, bench arms, VERIFIED.md.
