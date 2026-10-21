# M120 — the rest of M119's review

**Serves:** D-34 (depth), and the findings [M119](../M119/VERIFIED.md) left open: each was out
of M119's scope, or would have changed what a caller sees. No backlog row.

## What is true today

- **Two sequential chains of independent reads.** An external column's pin check
  (`ExternalColumn::read_rows_bytes`) awaits a `head` then a `get_tag` per object, one after
  another: 2 × (1 + objects) rounds before the rows. `Segment::scan` of a segment that is not
  one fixed-width field reads each dense field's vectors in turn: one round a field.
- **A write body, a peer part and a query are each parsed twice:** into a `Value` tree -- 32
  bytes a number -- then the tree into the type. M119 tried parsing once and took it out: the
  typed parse refuses a duplicate key the tree accepted (last wins), and skips an unknown
  field's value without checking it (`1e999`, invalid UTF-8), which the tree refused.
- **A dense leg decodes its segment's centroid table into a `Vec` a centroid**, every query.
- **`scope`** (order-by, aggregate) copies HEAD out of the bounded cache on a hit.
- **A fuzzy filter collects both strings' characters per row**, and its edit-distance
  computes a fresh row `Vec` per character; it takes the value's trigrams again per block.
- **`exq` scores with a branch on each sign bit; `lire` recentres column by column.** Spikes
  with no caller outside tests, carrying index findings 1 and 8.

## Delta

1. The pin checks run together, and a multi-field scan's field reads run together. Errors are
   judged in the order the serial code met them, so the error a caller sees is the same.
2. A body is **validated** by one pass that builds nothing -- every number range-checked,
   every string's UTF-8 checked, as the tree's parse checks them -- and is parsed into its
   type directly only when that pass found no duplicate key in any object. Anything else, a
   typed parse that fails included, takes the tree path as today: the same acceptance, the
   same errors, word for word.
3. The centroid table is read in place: probes and `<c, q>` from its bytes.
4. `scope` borrows the cached HEAD; it copies only what it returns.
5. A fuzzy predicate carries its value's characters and trigrams, and its distance keeps two
   rows.
6. `exq` branchless as M119's kernel is; `lire` recentres row by row.

**Not changed:** any answer, its order, any request count on a path that succeeds, any wire
format, any error a caller sees.

## Acceptance criteria

1. **Depth.** `pins_are_checked_in_one_round`: a column over three objects reads its rows in
   2 rounds (pins, then rows), not 7. `a_multi_field_scan_reads_its_fields_together`: a scan of
   a three-field segment is 2 rounds. Both via `DepthCounting`.
2. **The same answer to every body.** `the_fast_parse_is_the_tree_parse`: over a corpus of
   bodies -- duplicates at each level, `1e999`, invalid UTF-8 in known and unknown fields,
   non-objects, truncations, every valid shape the suite writes -- each of the write, part
   and query parsers returns what the tree path returns, error text included.
3. **The centroid table in place agrees.** `the_table_in_place_is_the_decoded_one`: probes and
   dots from the bytes equal the decoded table's, to the bit.
4. **Fuzzy unchanged.** `the_needle_is_the_strings`: `within` with two rows against the old
   one, over random strings and every `k` from 0 to 3.
5. M119's bench: every digest unchanged; no kind's p50 above 1.05 of M119's, each the median
   of three runs alternating with M119's binary (one run on this host moves ±15%).
6. `./scripts/gates.sh` passes.

## Test plan

| # | Test | Fails first because | Mutation it catches |
|---|---|---|---|
| 1 | `pins_are_checked_in_one_round`, `a_multi_field_scan_reads_its_fields_together` | 7 and 4 rounds | the join undone |
| 2 | `the_fast_parse_is_the_tree_parse` | duplicates refused, `1e999` accepted | the duplicate check removed |
| 3 | `the_table_in_place_is_the_decoded_one` | it does not exist | a stride off by one |
| 4 | `the_needle_is_the_strings` | it does not exist | a row not reset between characters |

## RA budget

Depth: an external read `2 × (1 + objects) + 1` → `2`; a multi-field scan `1 + fields` → `2`.
Requests unchanged where they succeed. On a refused pin, every object's `head` and `get_tag`
are sent where the serial code stopped at the first mismatch: up to `2 × objects` more,
on a path that fails. No LIST.

## Risks

- **A body the validator accepts and the tree refuses**, or the reverse: criterion 2's corpus.
- **The fast path slower than the tree** for a body of many small objects: the bench's writes.

## Tasks

- **M120.1** — all of it, and the ledger.
- **M120.2** — the review's minors: a multi-query told apart before any typed parse, the
  part's dispatch pinned by the equivalence test, the corpus widened.
- **M120.3** — the review's last sub-points, found reconciling every finding in
  [FINDINGS.md](FINDINGS.md): FDE planes kept a thread, `rows_at` moving rows out, a batch of
  cache misses admitted under one lock, patches moved out of the request.
- **M120.4** — the ledgers say what landed, and count it.
- **M120.5** — a cache key shared by every range of a call; tests for the two paths the
  findings appendix found unexercised by value (a cached range's bytes, a metered slice's).
