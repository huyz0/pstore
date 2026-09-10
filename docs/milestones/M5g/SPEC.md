# M5g — The dense index a fold never built

**Serves:** backlog item 12, which [M5f](../M5f/VERIFIED.md) opened by name — *"`Engine::seal`
writes no centroid table, so a folded segment carries **no dense index at all** and
`Engine::search` is exact brute force over a full `scan`. That, not identity, is why
`Engine::query` does not exist."*

**Depends on** [M3](../M3/SPEC.md)'s `vec_index::build_all`, which builds the whole thing and
which nothing in the write path calls.

## ⚠️ The failure this milestone exists to prevent

`Engine::search` scans **every document of every segment on every query** and scores them in
memory. It is exact, so no test of correctness fails — the ANN index, the RaBitQ codes, the
`Sq8` rerank rung and the centroid table are all built, tested, gated on recall, and **never
reached from a write the engine performed**. D-10's exact-scan threshold exists for indexes
*below* 25,000 rows; here it is every index at every size.

Two more, each silent:

1. ⚠️ **`vec_index::build_all` calls `finish()`, and `Engine::seal` calls `try_finish()`** —
   and the two carry comments that disagree. `seal`'s says *"`finish` is lossy for anything the
   format cannot hold and the fold used it, so a document the writer could not store was
   written as nothing and reported as durable."* Delegating without a `try_` variant hands the
   fold back the exact bug that comment records fixing. The refusal it loses is not
   `check_storable`'s — it is `try_finish`'s **index-budget** refusal, "too wide to open in one
   round trip", which `check_storable` knows nothing about.
2. ⚠️ **Above the threshold, a segment's row order becomes list order** — rows are written in
   the order their posting lists are read, which is what makes a probe one contiguous range.
   `compact`'s comment says *"In input order, so the merged segment reads back in the order the
   inputs would have. A merge that reorders is a merge that changes the answer."* That was
   written when the engine could not produce a clustered segment, and it stops being true at
   scale. ⚠️ **Below `exact_scan_threshold` = 25,000 rows nothing is clustered and the order is
   unchanged**, which is every fold any current test performs — so the comment is not wrong
   today and would become wrong silently. It is narrowed here, and the merged **set** is
   asserted at scale instead.
3. ⚠️ **The dependency runs the wrong way.** `pstore-index` dev-depends on `pstore-engine` for
   one test — a *cold query from HEAD*, which asserts an **engine** property using a
   hand-built HEAD. Engine gaining an index dependency makes that a cycle. The test moves.

## Delta

**`pstore-index`**
- `vec_index::try_build_all(..) -> Result<Built, FormatError>`, which is `build_all` through
  `try_finish`. ⚠️ `build_all` stays, and stays `finish`-based: its own comment is right for
  its own caller — a test fixture that has already read every document does want the lossy
  form, and reporting a width error at that layer is reporting it in the wrong place.
- `vec_index::centroid_key(segment)` → `<segment>.cen`, the third instance of a convention
  `text::dict_key` (`.tdict`) and `sparse::dict_key` (`.sdict`) already set. Derived, never
  discovered.
- ⚠️ `persisted.rs`'s `a_cold_query_from_head_costs_three_round_trips` **moves** to
  `pstore-engine`, and the dev-dependency on `pstore-engine` goes with it. It asserts depth
  from HEAD, which is the engine's property; it only lived here because the engine could not
  produce an indexed segment to assert it against.

**`pstore-engine`** — gains `pstore-index` and `pstore-query` as dependencies, which makes it
the top layer rather than a sibling of the index.
- `seal` builds through `try_build_all` and writes the centroid table beside the segment when
  there is one. ⚠️ **Written before the segment**, for the reason the sparse dictionary already
  is: a segment HEAD names whose sidecar is not there yet is a segment whose index cannot be
  reconstructed.
- ⚠️ `seal` writes a sidecar only when there is something in it. `Built` carries a text
  dictionary whenever a text field was named, but a corpus with no prose has no postings —
  and a `.tdict` object per fold for an index that has none is a PUT and an object that never
  reads back. The existing `if !text.postings.is_empty()` guard keeps its job.
- ⚠️ `seal` no longer builds the sparse and text sidecars itself. `build_all` builds all three
  over the **reordered** rows, and the clustering is what decides that order — three builders
  called over the input order produce three internally consistent indexes pointing at three
  different documents. That argument is already written on `build_all`; this is the first
  caller for which it is load-bearing rather than theoretical.
- `Engine::query(index, prefetch, fusion, top_k)` — reads HEAD, derives a `Target` per segment
  ref, and calls `pstore_query::query`.

⚠️ **`Engine::query`'s signature takes `pstore-query`'s types** — `Prefetch`, `Fusion`, `Hit`
— so they become part of the engine's public surface. That is the cost of the engine being the
composition point, and the alternative (a free function in `pstore-query` taking `&Engine`)
needs a public HEAD read that does not exist and should not be added for it.

**Does not add** — ⚠️ **freshness.** `Engine::query` sees **folded segments only**. Unflushed
and unfolded rows live in the memtable, which has no index, no rows and no segment ordinal, so
they cannot enter a `(segment, row)` fusion. `scan` and `search` still see them. This is a real
split in what a caller gets — indexed *or* fresh, not both — and criterion 6 pins it so it is a
stated limit rather than a discovered one. **Changing `Engine::search`** to use the index:
that swaps an exact answer for an approximate one and belongs behind the recall gate, with a
caller who asked. **A block size for `build_all`** — it hardcodes 64 and `ROWS_PER_BLOCK` is 64;
the duplication is noted, not fixed. **Per-index `Params`** — `Params::default()` for every
index, as every other caller does.

## Acceptance criteria

1. **A folded segment carries a dense index.** Above the exact-scan threshold, the segment
   HEAD names has `RaBitQ` and `Sq8` sections and a centroid object at the derived key.
2. ⚠️ **Below the threshold, no centroid object** — D-10's "scan me exactly", not a failure.
   Writing one anyway is a request and an object per fold no query reads.
3. ⚠️ **A compaction rebuilds it.** The merged segment has the index too, and its centroid
   object is at *its* derived key. Otherwise the first merge silently un-indexes an index.
4. ⚠️ **A fold below the threshold changes nothing.** `Params::default()`'s
   `exact_scan_threshold` is 25,000, and every existing engine test folds far fewer — so no
   centroid object, identity row order, and the whole existing suite green without an edit.
   This is the criterion that says the milestone is additive at the sizes anything is tested at.
5. ⚠️ **The sparse and text sidecars still address the right rows.** A hybrid corpus folded
   through the engine keeps every document's own vector and text, asserted by **document id**
   after asserting the row order **did** change — otherwise the fixture checks nothing.
6. ⚠️ **A segment too wide to open in one round trip is refused, not written.** A fold whose
   segment would exceed `INDEX_BUDGET` returns an error and commits nothing, rather than
   reporting durable a segment every query opens in two round trips.
7. **`Engine::query` answers from HEAD**, over every segment the index names, and a second
   fold changes the answer. Zero LIST.
8. ⚠️ **`Engine::query` does not see unfolded rows, and says so by test.** Write without
   folding: `scan` returns the row and `query` does not. The limit is pinned, not discovered.
9. ⚠️ **The centroid table is actually reached.** An indexed query moves **fewer bytes** than
   the same query with the table deleted, having first asserted the two answers are identical.
   ⚠️ Added after the mutation sweep: a wrong centroid key changes **no result at all**,
   because a missing table is how D-10 says "scan me exactly". Results cannot see it; bytes can.
10. **A cold indexed query from HEAD is ≤3 sequential round trips**, the assertion moved from
    `pstore-index` and now run against a segment the **engine** produced.
11. **No sidecar is written for an index that has none** — a vector-only fold writes no
    `.tdict` and no `.sdict`.
12. Region coverage ≥95% on the changed crates, mutation ≥80% on the changed modules, gates
    green, and `scripts/recall.sh` and `scripts/ndcg.sh` above their floors.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `a_folded_segment_carries_its_dense_index` | the centroid table built and never written, which leaves every query exact and slower with nothing failing |
| 2 | `a_compaction_rebuilds_the_dense_index` | `compact` sealing through the old path, which un-indexes on the first merge |
| 3 | the existing engine suite, unedited | the threshold ignored, which clusters every fold and reorders answers tests already assert |
| 4 | `a_hybrid_corpus_survives_the_reordering` | sparse or text postings built over the input order rather than the clustered order — internally consistent, pointing at the wrong documents |
| 5 | `an_over_wide_segment_is_refused_by_the_fold` | `try_build_all` reverted to `finish`, which reports durable a segment that costs a second round trip on every open |
| 6 | `a_query_answers_from_head_over_every_segment` | one segment queried instead of all, which returns a plausible top-k from a subset |
| 7 | `an_unfolded_row_is_visible_to_scan_and_not_to_query` | the limit quietly changing, in either direction |
| 8 | `a_cold_query_from_head_costs_three_round_trips` | a sidecar fetched in its own round rather than beside the footer |

⚠️ Criterion 4 is the one no smaller test covers: it is the only place the **row reordering**
meets the two sidecars the engine used to build itself.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| `fold`, per index | **+1** — the centroid object | unchanged | unchanged | 0 |
| `Engine::query` | 0 | **3** — HEAD, then the open round, then the legs | HEAD is 1; the open round is N segments × their sidecars | 0 |
| `Engine::scan` / `search` | unchanged | unchanged | unchanged | 0 |

⚠️ The write cost is one PUT per index per fold, and it does not scale with documents — the
rule bounds requests by nodes and bytes, and a centroid table is bytes.

## Risks

- **Clustering at fold time is CPU the fold did not spend before.** Below the exact-scan
  threshold nothing is clustered, which is most segments; above it, a fold now costs what
  `build_all` costs. Nothing here bounds it, and no gate measures fold latency.
- **The row order of every future segment changes.** Any caller holding a `(segment, row)` from
  before a re-fold is wrong — which is already true across compaction, and is why M5f scoped
  that identity to a HEAD snapshot.
- ⚠️ **`Engine::query` is indexed-but-stale and `Engine::search` is fresh-but-exact.** A caller
  must choose, and neither name says so. The gap is the memtable's, and closing it needs
  something that can score unindexed rows into a `(segment, row)` fusion.

## ⚠️ AMENDED after implementation — three findings the spec did not have

1. ⚠️ **Boundary replication duplicates ROWS, and a durable segment cannot have that.** A
   replicated vector goes into a second posting list, and rows are written in list order — so
   the document is written to the segment **twice**. Measured on the first attempt: 400
   documents folded and merged came back as **431 rows**, and it compounds on every merge
   because a compaction re-seals what it scanned. `Engine::scan` promises every row "exactly
   once". `seal` therefore clamps `replicas: 0` regardless of what a caller passed, loudly and
   with a test. ⚠️ The cost is real: M3's own table measures r@10 at p=2 as 0.961 with
   `1 x 0.10` replication against **0.844** without. Getting it back needs list membership that
   does not duplicate a row, which is a layout change and its own milestone.
2. ⚠️ **The layering comment in `persisted.rs` is overturned, not silently contradicted.** It
   said the joining layer "is `pstore-query` (layer 4) … `pstore-index` is layer 3 and may
   depend DOWNWARD on `pstore-engine`". Once the **fold** must build a dense index, engine →
   index is forced whichever way the query path is arranged, so that ordering cannot survive.
   The engine is the composition point; the comment is corrected where the test now lives.
3. ⚠️ **`centroid_key`'s correctness is invisible in the results.** Point it at an object that
   does not exist and every answer is identical — *exact*, because a missing centroid table is
   how D-10 says "scan me". The mutation survived every result assertion. What changes is the
   **bytes**, so `a_query_probes_rather_than_scanning` compares a probed query against the same
   query with the table deleted. Criterion 6 alone would have shipped a broken key.

## Tasks

| Id | Commit |
|---|---|
| **M5g.1** | The fold builds the index, and refuses a segment too wide to open |
| **M5g.2** | `Engine::query`, and the freshness limit it carries |

⚠️ Criterion 5's refusal is reachable through the fold: ~400 distinct int attributes make one
block's zone maps exceed `INDEX_BUDGET`, and `check_storable` at the write door lets them
through because there is nothing wrong with the *documents*.
