# M41 — A fresh view never matches another writer's row

**Serves:** [BACKLOG](../BACKLOG.md) row 56, whose last item [M40](../M40/VERIFIED.md) left open:
"a default engine's fresh view can match a `body` writer's ordinary `text` attribute until a
fold" ([M37](../M37/VERIFIED.md) code review, minor 2).

## What is true today

Measured while planning this milestone, with a probe that was not committed. An index exists
with an empty text field. A `body` engine writes and flushes a row carrying `revenue` under an
ordinary attribute named `text`, and no `body` attribute. A default engine and the `body`
engine each run a text query for `revenue`, over `text` and over `body`, before and after a
fold by the default engine. **All eight answers are empty.**

- **A fresh view holds only its own engine's unfolded rows.** The default engine's never holds
  the `body` writer's row, so it cannot match it.
- The `body` engine's fresh view indexes `body`, its own field, while the schema records none.
  The row carries nothing there.
- After the fold, the row is stamped `body` with no text under it, so nothing is recorded and
  nothing is indexed (M37).

So the item describes no reachable behaviour. The fresh view's fallback to its own field is the
row's writer's field, because the only rows it holds are its own: each engine has its own
memtable, filled only by its own writes and flushes (spec review confirmed this on every path,
including replicas, recovery, and two engines on one lane in one process).

⚠️ **Out of scope, and not this item** (spec review): a row visible in its own engine's fresh
view can be quarantined at the fold.
- Example: on an index with an empty text field, a default engine's text row and a `body`
  engine's `body` row meet in one fold. The first row's field wins (`filled`), so the other is
  quarantined.
- That is M25's contract for every conflict a fold finds, the same as for a width or a metric:
  an unfolded row is served until the fold sets it aside, counted and exportable.
- No server reaches the text-field case, since every server engine uses the default field.

## Delta

**A test pins that no engine matches another writer's row, before or after a fold, and row 56
is closed.** No source changes.

## Acceptance criteria

1. **No answer changes across a fold.** For the scenario above, all eight answers are equal
   before and after the fold, and are `Ok([])`. The test compares the results, never unwraps
   them, so an answer turning into an error is a failure, not a panic (spec review).
   - **Mutation:** the fresh view's fallback using the default field instead of its own
     engine's. The `body` engine's fresh view then matches the row over `text` before the fold
     (`["r"]`), and its query over `body` errs ("no such vector field").
   - `before_any_text_the_fresh_segment_indexes_the_process_field` already kills that mutant
     (spec review). The new test pins this scenario besides.
2. **And the agreement holds for text that is indexed.** A `body` engine writes `revenue` under
   `body` into the same kind of index. Its fresh view finds the row over `body` before the
   fold, and the fold, run by a default engine, records `body` and still finds it.
3. **Gates.** `./scripts/gates.sh` is green.

## Test plan

| # | Test | Red because / mutation it catches |
|---|---|---|
| 1, 2 | `a_fresh_view_never_matches_another_writers_row` (`pstore-engine/tests/schema.rs`) | ⚠️ green on the parent by design: it pins the absence of the defect the item described. Its red is the mutation named in criterion 1 |

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Any | 0 | 0 | 0 | 0 | unchanged: no source change |

## Risks

- **A closure by measurement can be wrong if the probe missed a path.** The probe and the test
  cover both engines, both fields, before and after a fold, and spec review checked every path
  that fills a memtable. The test keeps it from regressing silently. Revealer: the test.

## Tasks

- **M41.1** — The test, its mutation seen red, the ledger, row 56 closed, and the roadmap row.
