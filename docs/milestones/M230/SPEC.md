# M230: string and bool equality prunes blocks

## Serves
Item 1 of [the column-storage adoption list](../../comparisons/column-storage-adoption.md)
("string and bool equality pruning"). [file-format-and-layout.md](../../research/05-storage-engine/file-format-and-layout.md)
requirement 3's zone maps, extended to the types they skip. D-34 and D-35 unchanged.

## Delta
Today `could_admit` prunes a block only through an int, float, uint64 or datetime zone, or a
pattern's trigram sketch (M15.2). `category = "shoes"` and `archived = true` keep every block,
and the benchmark (`scripts/columns.sh`, new here) measures it: at 20,000 rows every
string and bool equality reads all 40 blocks, 4.58 MB, even for a value no row holds.

After, a new additive section, `Section::Equality` (id 18), in the meta region beside the
trigram sketch, and read by the open round:
- per **bool** attribute, two bits a block: some row holds `true`, some row holds `false`;
- per **string** attribute, a Bloom filter a block over its string values (three probes,
  one 64-bit hash of the value's bytes), all filters one width: the widest power of two
  from 2,048 down to 64 bits whose section fits what the block index and the trigram sketch
  leave of `INDEX_BUDGET`. Left out: an attribute whose blocks hold on average more than
  half its distinct values (a uniform `category` prunes only values no row holds), and, at a
  width, one whose busiest block's values would fill more than a quarter of it (a filter
  mostly ones). At each width the attributes with the fewest values are kept and the rest
  dropped until the section fits, before a narrower width is tried.
- A block where one row of a sketched string attribute holds a datetime, uuid or bytes is
  marked **open** for it: a string literal can equal such a row (M9h.3, M171), and its
  string form is not what was hashed. An open block is never pruned.

`could_admit` then prunes, for `Cmp(a, Eq, Str|Bool)` and each `In` member, a block whose
filter proves no row holds the literal. An attribute the section does not name, a segment
without it, an open block, and every other predicate prune exactly as before.

Not done, and why: the comparison's item 1(a), the trigram sketch for `Eq`, is **unsound**:
the sketch hashes only `Str` rows, so a block whose match is a uuid or datetime row read
from a string literal would be dropped. ClickHouse's exact `set(N)` is not built: at the
widths that fit, a Bloom filter answers the same blocks for fewer bytes (criterion 5's table).
No format version: a reader that has never heard of id 18 skips it, as every additive
section is skipped. Block sizes never change: the section takes only spare budget.

## Acceptance criteria
1. `could_admit` over a segment whose blocks hold disjoint `customer` values and a clustered
   bool keeps exactly the blocks that hold the literal (`Eq` and `In` on strings, `Eq` on
   bools), and the masks equal a reference over the rows.
2. Soundness: over random blocks of mixed strings, bools, datetimes, uuids, bytes, numbers
   and arrays, and random literals of every kind, `Eq`, `In`, `Not`, `And`, `Or` never prune
   a block holding a matching row; datetime and uuid rows matched by string literals included.
3. A segment written without the section (`without_equality_section_for_test`) and the
   M15.2 sketch fixture open and answer exactly as before; the trigram sketch's width is the
   same with and without the new section at the bench's 20,000 rows.
4. The meta region still fits `INDEX_BUDGET` (`scripts/depth.sh` green; 200 string
   attributes of unique values keep the plain segment's blocks and fit the budget), and a
   filter whose values would fill more than a quarter of the widest width, or an attribute
   whose blocks each hold most of its values, is not written.
5. `scripts/columns.sh` at 20,000 rows: the selective clustered string equality reads at most
   one tenth of main's blocks and bytes, an absent value reads none, and the uniform
   `category` filter's mask time stays within 10% of main's. Numbers in `VERIFIED.md` as
   `provisional`, with write time and meta-region bytes before and after.

## Test plan
- 1: `equality_prunes_blocks_without_the_literal` (pstore-query `tests/it`), red on main
  (keeps all blocks). Kills: Bloom probe ignored; bool bits swapped; `In` reading only its
  first member.
- 2: `equality_pruning_never_drops_a_matching_block` (pstore-query `tests/it`), seeded loop,
  thousands of cases. Kills: open marking dropped; datetime rows not opening a block.
- 3: `a_segment_without_the_equality_section_answers_as_before` and
  `the_equality_section_never_narrows_the_trigram_sketch` (pstore-format `tests/it`).
- 4: `many_string_attributes_never_change_the_blocks` and
  `a_filter_that_would_be_mostly_ones_is_not_written` (pstore-format `tests/it`), plus
  `depth.sh`. Kills: the saturation cut removed; the selectivity cut removed.
- Unit tests in `equality.rs` for encode/decode round trip and refusals of truncated bytes.

## RA budget
Unchanged requests and depth: the section is inside the 8 KiB suffix the open round already
reads. Data bytes fall by the pruned blocks.

## Risks
- Budget: at gate scale the spare after the block index is ~3.9 KB; more blocks or more zone
  names leave less, and the section quietly narrows or disappears. The bench prints its width.
- Memory: each open segment holds its section (at most the spare, < 8 KiB) beside its zones.

## Tasks
- M230.1: `scripts/columns.sh` benchmark and `Segment::block_zones`, baseline recorded.
- M230.2: `Section::Equality`, writer, reader, `could_admit`; tests; VERIFIED.md.
