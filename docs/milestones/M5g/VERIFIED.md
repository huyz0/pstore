# M5g — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a **mutation applied to the shipped code**, the named test run, and
the failure read. Commands: `cargo test -p pstore-engine --test dense_index`,
`--test engine_query`, `--test cold_depth`.

1. **A folded segment carries its dense index** — `a_folded_segment_carries_its_dense_index`:
   `RaBitQ` and `Sq8` sections, and a centroid object at the derived key. Before this, the ANN
   index, the codes, the rerank rung and the centroid table were all built, tested and gated on
   recall, and **never reached from a write the engine performed**.
2. **Below the threshold, no centroid object** —
   `below_the_threshold_no_centroid_object_is_written`. D-10 reads absence as "scan me
   exactly"; writing one anyway is a request and an object per fold that no query reads.
3. **A compaction rebuilds it** — `a_compaction_rebuilds_the_dense_index`, 400 rows and an
   index on the merged segment. Otherwise the first merge silently un-indexes an index.
4. **A fold below the threshold changes nothing** — the whole existing engine suite, unedited.
   `Params::default()`'s threshold is 25,000 and every existing test folds far fewer, so the
   row order is unchanged and `compaction_does_not_change_the_answer`'s **ordered** comparison
   still holds. ⚠️ That test is why this criterion exists: above the threshold rows are written
   in list order, and `compact`'s comment — *"a merge that reorders is a merge that changes the
   answer"* — stops being true. It is narrowed rather than deleted, and the merged **set** is
   what criterion 5 asserts at scale.
5. **The sidecars still address the right rows** — `a_hybrid_corpus_survives_the_reordering`:
   300 documents with a sparse field and prose, folded through the engine. The test asserts the
   row order **did** change (`assert_ne!`) before asserting the set is preserved and every
   document kept its own vector and text — otherwise it is checking nothing. This is the only
   place the reordering meets the two sidecars the engine used to build itself.
6. **An over-wide segment is refused, not written** —
   `an_over_wide_segment_is_refused_by_the_fold`: ~400 distinct int attributes make one block's
   zone maps exceed `INDEX_BUDGET`; the fold errors naming "round trip" and commits no HEAD.
   Observed red by reverting `try_build_all` to `finish`. ⚠️ This is not `check_storable`'s
   refusal — that guards the write door against documents the format cannot hold and knows
   nothing about segment width, which is why those documents reach `seal` at all.
7. **`Engine::query` answers from HEAD** — `a_query_answers_from_head_over_every_segment`: a
   second fold changes the answer and segment 1 contributes. Zero LIST. Observed red by taking
   only the first segment ref. ⚠️ The first fixture could not fail: `(i + j) % 13` repeats
   every 13 documents, so both segments held identical vectors and the tie-break `(segment,
   row)` hid whether segment 1 was read at all. Component 0 now rises with the index.
8. ⚠️ **`Engine::query` did not see unfolded rows, and said so by test** — **SUPERSEDED by
   [M5h](../M5h/VERIFIED.md), and recorded rather than quietly rewritten.** This criterion was
   met as written: the limit was pinned in both directions and then shown to resolve after a
   fold. M5h closed it — the memtable is now sealed into a segment that lives only in memory
   and queried alongside HEAD's — so the test that asserted the limit now asserts the
   agreement, as `an_unfolded_row_reaches_both_scan_and_query`, and
   `an_unfolded_row_reaches_the_indexed_query` is its counterpart on the new side. ⚠️ The test
   was **amended, not deleted**: a behaviour change recorded by a test disappearing is a
   behaviour change nobody can find later. `scripts/check-verified.py` is what caught this
   ledger still naming the old identifier.
9. ⚠️ **The centroid table is actually reached** — `a_query_probes_rather_than_scanning`.
   **Observed red, and it is the finding that matters most here:** pointing `centroid_key` at a
   nonexistent object survived every result assertion, because a missing table is how D-10 says
   "scan me exactly" — same answers, exactly, more slowly. The test compares **bytes** against
   the same query with the table deleted, having first asserted the two answers are identical
   so the comparison is like for like.
10. **A cold indexed query from HEAD is 3 round trips** —
    `a_cold_query_from_head_costs_three_round_trips`, moved from `pstore-index` and now run
    against a segment the **engine** produced rather than a fixture's hand-built one and a
    hand-written HEAD. It also asserts the centroid object exists, so the depth cannot pass by
    falling back to an exact scan.
11. **No sidecar for an index that has none** — `a_vector_only_fold_writes_no_sidecars`.
12. **Gates** — `./scripts/gates.sh` green, `scripts/ndcg.sh` **1.0000** against a 0.80 floor
    with the control at 0.5441, `scripts/recall.sh` PASS (none 0.2840, fast 0.7380, exact
    0.7390). Coverage: workspace **95.45%** regions via
    `./scripts/coverage.sh --fail-under-regions 95`; `pstore-engine/src/lib.rs` 94.56%,
    `pstore-index/src/vec_index.rs` 94.88%.

## What is not built, and named rather than omitted

- ⚠️ **Boundary replication, and the recall it was worth.** `seal` clamps `replicas: 0`
  because a replicated vector is written to the segment **twice** — measured at 431 rows for
  400 documents, compounding on every merge, against `Engine::scan`'s promise of "exactly
  once". M3's table puts the cost at r@10 p=2 **0.961 → 0.844**. Recovering it needs list
  membership that does not duplicate a row, which is a layout change. Pinned by
  `a_replicated_vector_is_still_one_row`, so the clamp cannot be removed quietly.
- ⚠️ **Freshness in the indexed path.** The memtable has no index, no segment and no row
  ordinal, so its rows cannot enter a `(segment, row)` fusion.
- **Changing `Engine::search`** to use the index: that swaps an exact answer for an approximate
  one and belongs behind the recall gate, with a caller who asked for it.
- **Per-index `Params`**, and a bound on fold-time clustering CPU. No gate measures fold
  latency, and above the threshold a fold now costs what `build_all` costs.
