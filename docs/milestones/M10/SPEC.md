# M10 — The served epoch

**Serves:** [BACKLOG](../BACKLOG.md) row 40, found at M9i.2's spec review. The query response's
`meta.epoch` says it is "the epoch the answer was computed against" (`QueryMeta`), and it is
not that.

## What is true today

- `run` fills `meta.epoch` from `engine.epoch()`: the last epoch **this process committed**.
  The field is correct only by coincidence, when this process made the latest commit.
  - A read-only process reports 0 while serving epoch 7.
  - A process whose last fold was epoch 3 reports 3 after another process has folded to 5,
    and it serves 5.
- So a `strong` answer cannot say what it reflects, and a client cannot pass `meta.epoch`
  back as `as_of` to repeat a read.
- `as_of` queries already report the epoch they asked for. That is correct and stays.

## Delta

**The engine says what it served.**
- `Answer` and `Ordered` each gain `epoch: Epoch`, the epoch of the manifest the rows came from:
  - a live query: the HEAD it read, including a HEAD re-read because this engine had pruned
    past the first read (M9c.1);
  - an `as_of` query: the asked epoch, which is what `Head::as_of` stamps on its manifest.
- Filled from the HEAD read the query already makes, so it costs **no request**.

**The server reports it.**
- `run` reports `answer.epoch` in `meta.epoch`, for a relevance query and for a `rank_by`
  order.
- A multi-query's `meta.epochs` follows, since each entry comes from `run`.
- `QueryMeta.epoch`'s doc is accurate as it stands. It gains one sentence: rows from this
  process's unfolded writes are newer than that epoch and are counted in `unfolded_hits`.

**Does not change:**
- a write's `epoch` (the process's last commit; a write reads no HEAD, by design);
- the index-metadata endpoint. Its `epoch` is HEAD's for a folded index. For an index that
  only this process holds unfolded, it falls back to the process's last commit. That is the
  same defect, but fixing it changes what `index_stats` returns, so it is BACKLOG row 42
  (spec review);
- any request count, depth or format.

## Acceptance criteria

1. **A read-only process.** Process A writes durably and folds, reaching epoch `e >= 1`.
   Process B, on another lane, has never committed. For B, `meta.epoch` is `e` for:
   - a relevance query;
   - a `rank_by` order;
   - a `strong` relevance query;
   - each entry of a multi-query's `meta.epochs`.
2. **A stale process.** A folds, reaching `e`. B writes and folds, reaching `e + 1`. A's
   relevance query and `rank_by` order both report `e + 1`.
3. **Round trip.** Passing B's `meta.epoch` back as `as_of` returns the same ids in the same
   order as the live answer, when nothing is unfolded.
4. **The engine.** On an engine that never committed:
   - `Answer::epoch` and `Ordered::epoch` equal HEAD's epoch;
   - an `as_of` query at `e - 1` returns epoch `e - 1` for both;
   - (spec review) a query of an index with no segment is stamped too, live and `as_of`: the
     empty-target early returns are built separately.
5. **No cost.** Criterion 1's queries issue exactly the requests they issued before. The
   existing count and depth tests pass untouched.
6. `./scripts/gates.sh` passes, and `./scripts/mutants.sh` over the diff misses 0.

## Test plan

| # | Fails first | Mutation it catches |
|---|---|---|
| 1 | reports 0 | the epoch taken from the engine, not the HEAD |
| 2 | reports `e` | the same, for a process that did commit |
| 3 | `as_of = 0` answers nothing | — (the field's contract) |
| 4 | new fields | the stamp dropped on the empty-target early return; `as_of` reporting HEAD's epoch rather than the asked one |

## RA budget

Unchanged: the epoch comes from a HEAD the query already read.

## Risks

- A query that returns early on "no segment" must still stamp the epoch. The empty-target
  early returns are constructed separately, and criterion 4 includes an empty index.

## Tasks

- **M10.1** — the engine's `epoch` fields, the server's report, and BACKLOG row 40 closed;
  criteria 1–6.
