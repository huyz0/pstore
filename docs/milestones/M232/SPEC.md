# M232: per-block aggregate statistics

## Serves
Item 3 of [the column-storage adoption list](../../comparisons/column-storage-adoption.md).
M12's aggregations; D-34 unchanged.

## Delta
Today an aggregation reads every block its filter's zone maps keep and folds each admitted row
(`pstore_query::aggregate`). Only an unfiltered `Count` of rows is answered without blocks, from
HEAD's row counts.

After, a segment may carry an **`Aggregates` section** (id 19), inline in the meta region the
open already reads, after the sketch and the equality section, in what they leave of the 8 KiB
suffix. Per numeric attribute, per block: how many rows hold it (`u32`) and their sum (8 bytes).
- Only an attribute that is, across the whole segment, **one numeric kind**: all `Int` or all
  `Float`. Then "holds the name" and "holds a number" are the same rows, and the block's zone
  map is its `Min` and `Max`. Strings, uint64s, arrays and mixtures are left out.
- An int sum is exact (summed in `i128`) or the attribute is left out; a float sum is the
  block's rows in row order, and left out when not finite.
- Names in order; when the section does not fit, the last names are dropped; none fitting
  writes none.

Query side: `Predicate::must_admit(zones, rows)` proves a block's every row admitted (a
comparison on a covered attribute every row holds, whose zone lies wholly on the admitted side;
`And` of such, `Or` with one such). `Aggregator::answers_block` (ungrouped, every aggregate's
attribute covered, a zone where `Min`, `Max` or `Avg` need one) and `offer_block` fold a block
from its statistics. `aggregate` folds such a block, with no deleted row and no shadowing, from
the open, and fetches only the others.

A setting, by the "a feature with a measured downside is a setting" non-negotiable:
`block_aggregates` in M231's storage settings (`PSTORE_BLOCK_AGGREGATES`, index override),
default on if criterion 4 holds.

Unchanged: grouped aggregations; filters `must_admit` cannot prove; segments without the
section, and every reader of them; requests and depth.

## Acceptance criteria
1. Equal answers: over seeded segments with deletes, with and without rows held unfolded, every
   aggregate (`Count`, `Count(attr)`, `Sum`, `Min`, `Max`, `Avg`) over int, float, sparse,
   mixed and string attributes answers the same under 300 random filters (`Cmp` of every op on
   ints, floats, extremes and strings, under `And`, `Or`, `Not`) with the section as without.
2. Fewer bytes: an unfiltered sum and a sum under a filter that admits every block read under
   half the bytes they read without the section.
3. The section: counts and sums round-trip; mixed kinds, uint64, an overflowing int sum and a
   non-finite float sum are left out; the budget keeps the first names that fit; every
   truncation, a trailing byte, an unknown kind and a bad name are refused.
4. `./scripts/columns.sh` (`provisional`), M231 against M232: segment and meta bytes, write
   time, and each aggregation's GETs, bytes and time. Default on only if no aggregation's bytes
   or time rises by more than 10% and a filter admitting blocks whole reads fewer bytes.
5. Both modes: server tests whose read counts were measured without the section pin
   `block_aggregates: false` and keep their assertions.

## Test plan
- 1, 2: `block_statistics_answer_what_the_rows_do`,
  `a_filter_admitting_blocks_whole_reads_fewer_bytes` (engine `tests/it/aggregate_stats.rs`).
  Kills: `must_admit`'s `Lt` as `Lte`; its "every row holds it" check dropped; `Or` admitting
  always; `Count(attr)` counting rows; a deleted row's block answered; an answered block also
  fetched; extremes dropped; a grouped spec answered; a shadowed segment answered.
- 3: unit tests in `aggregates.rs`. Kills: the mixed-kind check dropped.
- 4: the script. 5: `aggregate::a_refused_strong_aggregation_is_not_run_twice`.

## RA budget
Unchanged requests and depth. A filter admitting blocks whole fetches fewer blocks, so fewer
bytes and, where the kept blocks were not neighbours, fewer GETs.

## Risks
- A float total built from block sums can differ in the last bits from one built row by row,
  as two segments' partials already do.
- Rows held unfolded shadow every folded row, so no block is answered while a process holds
  any (shown by criterion 1's unfolded half answering by rows).
- The section competes for the suffix with the sketch and the equality section: it takes what
  they leave, never theirs.

## Tasks
- M232.1: the section, writer and reader, `must_admit`, the aggregation fast path, tests, bench
  arm, VERIFIED.md.
