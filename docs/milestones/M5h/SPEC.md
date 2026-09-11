# M5h — The rows the indexed query cannot see

**Serves:** backlog item 15, which [M5g](../M5g/VERIFIED.md) opened by name — *"`Engine::query`
is indexed-but-stale and `Engine::search` is fresh-but-exact. A caller must choose, and neither
name says so."*

**Depends on** [M5f](../M5f/SPEC.md)'s `(segment, row)` identity and [M5g](../M5g/SPEC.md)'s
`Engine::query`, which is the thing that cannot see them.

## ⚠️ The failure: the freshness layer exists and the indexed path does not reach it

`Memtable`'s own doc states the promise: *"This is the freshness layer: **visibility does not
wait on the fold**, so the flush interval never enters the time-to-searchable budget."* `scan`
and `search` honour it. `query` does not — it reads HEAD and sees only what the last fold
published, so a document written a moment ago is **absent from an indexed answer and present
in an exact one**, with no error and nothing in either name to say which you get.

`an_unfolded_row_is_visible_to_scan_and_not_to_query` pins it today, which makes it a stated
limit rather than a surprise. It is still the wrong answer.

## ⚠️ Why it is not a one-line fix, and what review changed about the shape

Memtable rows have **no segment and no row ordinal**, so they cannot enter a `(segment, row)`
fusion as they are. The first draft of this spec had the engine score them **exactly, in
memory** and splice the results into each retriever's union. Spec review rejected it:

⚠️ **That needs a second implementation of every scorer, and BM25 twice is how the two come to
disagree.** `TextIndex::search` scores over built postings; scoring raw documents means a
second BM25 that must agree with the first *exactly*, or criterion 4 fails by construction —
the same hazard `Hit`'s own doc names about inventing an identity twice. Dense and sparse have
the same problem in smaller form.

**So the memtable becomes a segment.** `Engine::query` seals the unfolded rows into an
in-memory segment — the same `try_build_all` a fold uses, into a private `MemoryStore` — and
adds it to the target list. Then there is **one** implementation of everything: one BM25, one
dense path, one sparse path, one fusion, one `(segment, row)` space. What makes this affordable
is that it is **cached on a generation counter** and rebuilt only when the memtable changes, so
the cost lands on `write` and `flush` rather than on every query.

Two things still have to line up:

1. ⚠️ **The statistics must include it.** M5c measured that global IDF changes the top-1, and a
   fresh segment scored against statistics gathered only from the folded ones is that defect
   one corpus-slice down. `Stats::merge` already takes any number of summaries; the fresh
   segment's dictionary contributes like any other.
2. ⚠️ **One fusion over the union.** RRF is blind to score magnitude, so fusing folded and
   fresh separately and merging gives the memtable's best hit the corpus's best hit's credit —
   the failure M5f's `fusion_happens_once_over_the_union` pins one level down. Because the
   fresh rows are a target like any other, this comes for free rather than being arranged.

⚠️ The caller still cannot resolve the fresh ordinal against HEAD, so `Engine::query` returns
an `Answer` naming which ordinal it is and the documents behind it.

## Delta

**`pstore-engine`**
- `Engine::query` seals the index's unfolded rows into an in-memory segment, queries it
  alongside HEAD's, and returns `Answer { hits, unfolded, unfolded_at }`.
  ⚠️ A struct rather than a bare `Vec<Hit>`, because `Hit { segment: unfolded_at, .. }` indexes
  **nothing in HEAD** and a caller resolving it against the segment list would be wrong.
- The fresh segment is **cached on a generation counter** bumped by `write`, `flush` and
  `fold`, so a query rebuilds it only when the memtable actually changed.

**Does not add** — ⚠️ **resolving hits to document ids.** That is a block fetch per segment and
would make `Engine::query` **four** sequential round trips, over the corpus's budget of three.
The handle stays `(segment, row)`. **A second scorer for any modality** — that is what this
design exists to avoid. **Changing `Engine::search`**, which stays fresh and exact and is still
the right answer for a caller who wants no approximation. **Durability for the fresh segment** —
it lives in a private `MemoryStore` and is rebuilt from the memtable, which is already durable
in bundles once flushed.

## Acceptance criteria

1. ⚠️ **An unfolded row reaches the indexed query.** Write without folding: the row appears in
   `query`'s answer, at `unfolded_at`, with its document in `unfolded`. This inverts
   `an_unfolded_row_is_visible_to_scan_and_not_to_query`, which is **amended, not deleted** —
   it becomes the assertion that `query` and `scan` now agree about that row.
2. ⚠️ **It is ranked against the folded corpus, not beside it.** A fixture where the fresh row
   belongs **in the middle** of the folded ranking comes back in the middle — not first, which
   is what stapling two answers together produces, and not last.
3. ⚠️ **The text statistics include the fresh rows.** Over a corpus whose unfolded slice shifts
   a term's document frequency, the ranking equals the ranking of the same documents **all
   folded** — and differs from the same query scored with segment-only statistics. Both halves,
   for the reason M5f's ledger records: equality alone passes wherever the two agree.
4. ⚠️ **Folding changes nothing, below the exact-scan threshold.** The same query before and
   after a fold of the same rows returns the same documents in the same order — the criterion
   that says the freshness layer is *invisible* rather than merely present, and what makes an
   answer trustworthy across a fold that lands mid-flight.
   ⚠️ **Scoped to below the threshold, and the scoping is a review finding rather than a
   convenience.** Above it a folded segment is searched by ANN probe and the fresh one, being
   small, is scanned exactly — so a row the probe would miss is present before the fold and
   absent after, and the first draft's unscoped version of this criterion contradicted this
   spec's own risk about exactly that. Below the threshold both halves are exact and the
   claim is total.
5. **Depth is unchanged at 3**, and zero LIST. Scoring in memory costs no request.
6. ⚠️ **`unfolded_at` indexes the returned documents and nothing else.** A hit at that ordinal
   resolves against `unfolded`; every other ordinal indexes HEAD's segment list. Asserted in
   both directions, because a caller indexing the wrong one gets a plausible wrong document.
7. **An index with nothing unfolded behaves exactly as it does today** — no fresh target is
   added at all, `unfolded` is empty, and the hits are identical to M5g's. ⚠️ An *empty* fresh
   segment must not be added: it would occupy an ordinal for nothing, and every other ordinal
   is a number a caller resolves against HEAD.
8. ⚠️ **A fresh segment that cannot be built fails the query.** If `try_build_all` refuses the
   memtable, `query` errors. It must **not** fall back to the folded-only answer: that is
   silently returning the stale result this milestone exists to remove, which is worse than an
   error a caller can see. Asserted against a memtable wide enough to be refused.
9. **The cache rebuilds when the memtable changes and not otherwise** — a generation counter
   bumped by `write`, `flush` and `fold`, per index. ⚠️ `flush` moves rows from pending to
   durable without changing what they are, so it rebuilds needlessly; bumping on it anyway is
   the safe direction and is cheaper than reasoning about which mutations matter.
10. Region coverage ≥95% on the changed crates, mutation ≥80% on the changed modules, gates
   green, and `scripts/ndcg.sh` and `scripts/recall.sh` above their floors.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `an_unfolded_row_reaches_the_indexed_query` | the memtable not collected — the defect today, and every other test here passes with it |
| 2 | `a_fresh_row_is_ranked_against_the_corpus_not_beside_it` | the fresh rows fused as their own leg or appended after, which gives the memtable's best hit the corpus's best hit's credit |
| 3 | `the_statistics_include_the_unfolded_rows` | the contribution dropped, which is M5c's global-IDF defect one corpus-slice down |
| 4 | `a_fold_does_not_change_the_answer_below_the_threshold` | the fresh rows scored by a different path from the folded ones — the two must agree, and only this compares them |
| 5 | `an_indexed_query_is_still_three_rounds_and_no_list` | a fetch introduced to read what is already in memory |
| 6 | `the_unfolded_ordinal_indexes_the_returned_documents` | `unfolded_at` off by one, which returns a plausible wrong document |
| 7 | `an_index_with_nothing_unfolded_is_unchanged` | an empty fresh segment that shifts every other ordinal |
| 8 | `a_memtable_that_cannot_be_sealed_fails_the_query` | a fallback to the folded-only answer, which is the stale result this milestone removes, returned silently |
| 9 | `the_fresh_segment_is_rebuilt_only_when_the_memtable_changes` | the cache never invalidated, which serves a fresh answer that stops being fresh |

⚠️ Criterion 4 is the one that cannot be faked: it compares the fresh path against the folded
path over the same rows, so any difference in how the two are scored shows up as a reordering.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| `Engine::query` | 0 | **3** — unchanged | unchanged | 0 |
| `Engine::scan` / `search` / `fold` | unchanged | unchanged | unchanged | 0 |

⚠️ Unchanged because the memtable is already in memory. What grows is **CPU per query**, with
the size of the unfolded window.

## Risks

- ⚠️ **`query` rebuilds a segment when the memtable changes.** The window is bounded by the
  flush and fold intervals in normal operation and by **nothing** if a fold is failing — so an
  index whose folds are erroring gets a growing memtable, a growing rebuild, and nothing here
  reports either. The generation cache moves the cost to writes; it does not bound it.
- ⚠️ **The fresh half is exact and the folded half is not, above the threshold.** A fresh row
  competes *advantaged* against a folded one the probe would have missed, and criterion 4 is
  scoped below the threshold for exactly that reason. The effect is bounded by the memtable's
  share of the corpus, which is small by construction and unbounded by anything if folds stop.
- ⚠️ **Building a segment is not free of the format's refusals.** `try_build_all` can refuse —
  a document too wide, a second sparse field — and a refusal here would make **querying** fail
  on rows that `write` already accepted. `check_storable` guards the write door, so the shapes
  that reach the memtable are storable; the width refusal is the one that could still fire, on
  a memtable large enough that its own zone maps exceed the budget.
- **`Answer` is a breaking change** to `Engine::query`'s return type, one milestone after it
  landed. Deliberate: a bare `Vec<Hit>` cannot express an ordinal that does not index HEAD.

## Tasks

| Id | Commit |
|---|---|
| **M5h.1** | `query` scores caller-supplied fresh rows into the same fusion |
| **M5h.2** | `Engine::query` supplies the memtable, and says which ordinal it is |
