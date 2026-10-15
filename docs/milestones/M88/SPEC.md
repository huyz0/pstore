# M88 — Parquet columns that hold nulls

**Serves:** backlog row 74's remainder, after [M84](../M84/SPEC.md).

## Decision

A null row has no vector, so it keeps its **number** and has no vector: the column's row space is
unchanged and the null rows are **gaps between runs**. No bitmap and no format change were needed:
a page's definition levels (M84 read them to prove "all present") now give the **runs of present
rows**, and each run is one extent whose offset is the page's values start plus
`rank × width`, `rank` being the present rows before the run in that page. `ExternalColumn`
learns gaps; the codes blob (`PXI5`) already stores `rows` and the extents, so the gaps are
implied by them and an old blob without gaps reads as before.

- `ExternalColumn::from_runs(.., rows, extents)` accepts ordered, non-overlapping runs with gaps
  and a trailing gap; `from_extents` keeps insisting on contiguous runs from row 0.
- `is_present(row)`; `row_range` and the compressed-page lookup answer `None` for a gap;
  `read_rows` of a null row is `ExternalError::Absent(row)`, never zeros.
- `ExternalIndex::build` writes a zero code for a null row (so a code's position is still its row)
  and `search` skips it; a filter cannot resurrect it.
- ⚠️ **Bounded against a hostile null pattern:** a page splits into at most 2^20 runs and a column
  into at most 2^22 extents, else it is refused.
- ⚠️ **Still refused:** a *compressed* page that holds nulls (its values cannot be ranked);
  dictionary-encoded optional columns; nested or repeated columns; a v2 page whose header null
  count disagrees with its levels.

## Acceptance criteria

1. `a_column_with_nulls_reads_back_every_present_row`: the real pyarrow `optional-nulls.parquet`
   (every tenth row null) pins; exactly the present rows are present and have a range, every
   present row equals the generator's floats, a null row is `Absent`, and runs have gaps.
2. `runs_with_gaps_are_checked`: gaps and a trailing gap are accepted; out-of-order, overlapping,
   past-the-column and empty runs are refused; `from_extents` still refuses gaps.
3. `levels_give_the_runs_of_present_rows_exactly` and
   `a_page_of_alternating_rows_is_refused_past_the_run_cap` (unit): the runs for RLE, bit-packed
   and mixed levels, leading and trailing nulls, padding bits, and the run cap at and past its edge.
4. `an_index_over_a_column_with_nulls_finds_the_exact_neighbours`: build, reopen (blob
   byte-identical) and search equal the exact cosine top 10 **over the present rows** for five
   queries (two of them null rows' own vectors); no null row is ever returned, with or without a
   filter.
5. `damaged_definition_levels_are_refused` and
   `an_optional_column_that_cannot_be_read_in_place_is_refused_by_name`: damaged levels are
   refused by name; compressed and dictionary-encoded optional columns still are.
6. **Gates:** `./scripts/gates.sh` passes; the mutation sweep of the changed lines misses 0.

**Not covered:** nested columns; a maximum definition level above 1; nulls in a compressed page.
