# M8m — `pstore-format`: the layout's two lengths, a zero row width, and 16 equivalents

**Serves:** **D-111**. The nightly on `0337112` (run 36112016404) missed 4 in `reader.rs`, with
two shards cancelled. Re-measured here: every `pstore-format` mutant against the crate's own
tests ([`sweep/crate-only.txt`](sweep/crate-only.txt), 776, 55 of ours missed), then those
55 against every package that can see the crate ([`sweep/workspace-recheck.txt`](sweep/workspace-recheck.txt)):
**24** survive — 8 in `reader.rs`/`lib.rs`, 16 in `sparse.rs`. Line numbers are the measurement's.

## Delta

**Tests (7 mutants), each value exact:**
- **A segment is its blocks, its index and its footer** (`lib.rs` `145:5` `index_section_len`
  → `Some(0)`, `Some(1)`; `reader.rs` `346:9` `data_end` → `0`, `1`): for attribute-only
  segments of 1, 50 and 600 rows, `data_end + index_section_len + footer == len`, the footer
  being `SUFFIX_FETCH − INDEX_BUDGET`. The existing tests only bounded both from one side.
- **A `Vectors` section with no rows** (`382:35` guard → `true`, `382:40` `>`→`>=`): `with_section`
  is public, so this segment exists. `vector_rows(&[0])` and `scan` are empty, with 0 reads;
  the mutants divide by zero.
- **A `Vectors` section narrower than a row does not displace the postings** (`591:82`
  `>`→`>=`). `scan` fetches blocks, vector rows and sparse postings in one `get_ranges` and
  finds the postings **by position**. A row width of 0 makes every vector range empty, and
  `get_ranges` neither fetches nor **returns** an empty range, so the postings would be read a
  slot late: the mutant fails `Truncated`. The test: 20 sparse rows with a 2-byte `Vectors`
  section scan exactly as without it.

**Rung 1 — rewritten so the mutant does not exist (3):**
- `reader.rs` `447:25` `||`→`&&` in `vector_rows`: under the default store equivalent (empty
  ranges are not fetched or returned), but a store pairing results with requests by position
  would misplace them. Split into two early returns — `per_row == 0`, then `rows.is_empty()`
  — each standing alone, so there is no `||`.
- `sparse.rs` `406:33` `max > 0.0`→`>=` in `put_impact`: the guard is deleted. A zero `max`
  means every impact is zero, and `0.0 / 0.0` is NaN, which the saturating cast makes `0` —
  the byte the guard produced.
- `sparse.rs` `407:64` `+ 128`→`- 128`: `as u8` wrapped both into the same byte. Now
  `u8::try_from(v + 128)`, which always fits for `v` in `−127..=127`; its `-` and `*` mutants
  fail `a_sparse_field_round_trips`.
The doc comment that "recorded rather than chased" the last two is replaced.

**Excluded — equivalent (14):** every `|`→`^` in `f32_to_f16` and `f16_to_f32`. Sign, exponent
and significand are disjoint bits, so OR is XOR; the functions are pinned exhaustively by
`f16_encodes_the_nearest_representable_value`. Named by line and column in
`.cargo/mutants.toml`, like M8h's: no test can back an equivalence, and a new `|` reappears.

**Found, not fixed here (out of scope, `pstore-blob`/`pstore-cache`):** `get_ranges` returns
fewer buffers than ranges when one is empty, and `pstore-cache`'s `get_ranges_as` zips its
misses against the inner result, so one empty miss would admit bytes under the wrong range.
No caller passes an empty range today — the `591` guard is part of why.

**Does not change:** any byte `SegmentWriter` or `sparse::build` emits, or any value a reader
returns. ⚠️ **Amended at review:** the public `sparse::write_list` with a `U8` `max` below the
list's largest `|w|` — which nothing in the tree passes — now saturates (`max = 0`, `w = 1`:
byte 255, was 128). Its doc now states the precondition `build` already meets.

## Acceptance criteria

1. Each test above passes, and each named mutant was seen failing it.
2. `vector_rows` has no `||` in its early return; `put_impact` has no `max > 0.0` and no
   `as u8`; each rewrite's new mutants are caught.
3. `cargo mutants --list` over `sparse.rs` drops by exactly the 14 f16 `|`→`^` mutants.
4. Every `pstore-format` mutant against its own tests, then any survivor against every
   package that can see it: **0 missed**. Timeouts count as caught.
5. `./scripts/gates.sh` passes.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | each named mutant, one at a time | the two lengths, the zero-row guard, the zero-width guard |
| 2 | each rewrite's own mutants, by hand | a wrong offset byte, a dropped zero-width return |
| 3 | the list without the exclusion | an exclusion broader than the 14 |
| 4 | the measurement: 24 missed | all of the above |

## RA budget

Unchanged: no request is added or removed on any path that has rows to read.

## Risks

- `put_impact` now leans on NaN casting to 0. That is defined Rust (saturating casts), and
  the byte for an all-zero term is unchanged; `a_sparse_field_round_trips` covers the rest.
- The exclusion is by position, so an edit above it un-hides the 14. That is the intent.

## Tasks

- **M8m.1** — the measurement, the tests, the rewrites, the exclusion, this ledger.
