# M3c — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a **mutation applied to the shipped code**, the named test run, and
the failure read. Commands: `cargo test -p pstore-format --test index_rows` and
`cargo test -p pstore-engine --test replication`.

1. **A replicated segment holds each document once** —
   `a_replicated_segment_holds_each_document_once`: `row_count()` 300 for 300 documents,
   `scan` returns 300, and a compaction of two such segments returns 600 rather than more.
   The criterion M5g's clamp existed to satisfy, now met with replication **on**. Observed red
   by pushing blocks over the index order.
2. **The codes carry the replicas** — `the_codes_carry_the_replicas`: the index row count
   exceeds the data row count, the mapping is that long, and every entry names a row the
   blocks have. Without it criterion 1 passes over a build that silently dropped replication.
   ⚠️ **The first fixture replicated nothing and asserted the opposite of what it claimed.**
   Its vectors were a lattice — `(i * 7 + j * 13) % 97` — and every point on a regular grid is
   decisively closer to one centroid than any other, so `augment` added no entries at all.
   Replication exists for vectors *between* centroids; the fixture is pseudorandom now.
3. ⚠️ **A replicated document appears once in a top-k** —
   `a_replicated_document_appears_once_in_a_top_k`. **Written because a mutation survived**:
   removing the deduplication changed nothing, because no test here probed a replicated index
   and looked at the ranking. Observed red by deduplicating on the index row instead of the
   data row, which returns the same document twice and comes back short.
4. ⚠️ **BM25 statistics count each document once** —
   `replication_does_not_change_the_bm25_statistics`: `doc_count`, `total_tokens` and every
   term's `df` equal between a replicated and an unreplicated build of the same documents.
   **This defect has been live since M3** — the sidecars were built over the expanded rows, so
   a replicated document was counted twice in the corpus size and in its terms' frequencies.
   Nothing caught it: the recall gate builds no text field and the ndcg gate does not
   replicate. Observed red by building the sidecars over the index order.
   ⚠️ It was **not** observed red before the fix, because the fix landed in M3c.2 with the row
   split it is part of; the mutation is how it was verified, and this ledger says so.
5. **Recall and bytes at small `p`** — `cargo run --release -p pstore-index --example recall --
   --replicas`, both corpora. Clustered, `replicas 1×0.05`: **p=2 recall 0.9610 at 0.288
   MB/query** against unreplicated **p=2 0.8890 at 0.248 MB** — 7.2 points for 16% more bytes —
   and against unreplicated **p=4 0.9680 at 0.429 MB**. ⚠️ At the default `p=8` replication
   buys **0.0000** and costs bytes, which is why criterion 6 exists.
6. ⚠️ **The shipped configuration is byte-identical** — `an_unreplicated_segment_is_byte_identical`
   asserts a writer told the identity mapping produces exactly the bytes of one never told,
   and the engine's default is now an explicit measured `replicas: 0` rather than a clamp.
   Observed red by writing the identity mapping as a section, which changes the bytes of every
   segment that does not replicate. `scripts/recall.sh` and the full suite are unmoved.
7. ⚠️ **Strides are derived from the right count** — `every_row_decodes_to_its_own_vector`,
   asserted against the **input** vectors, because a stride taken from the wrong count is
   self-consistent and wrong. Observed red by striding `Vectors` by the index row count.
   ⚠️ **The contract here is not what M3c.1 first wrote.** `Vectors` is full precision — 1,536
   bytes a row at 384d against 64 for the 1-bit code — so it is the section replication must
   **not** duplicate. `RaBitQ` and `Sq8` stride by the index row count; `Vectors` and the
   blocks stride by `row_count`. The M3c.1 test encoded the opposite and was corrected, not
   weakened: it still asserts every row decodes to its own document.
8. **Gates** — `./scripts/gates.sh` green, `./scripts/recall.sh` PASS. Mutation over
   `vec_index.rs`, in the `dev` container: **220 mutants, 109 caught, 5 missed, 106 unviable**
   — 95.6% of viable, above the 80% floor.
   ⚠️ **One of the five was real, and it named a hole this milestone did not create.** Nothing
   in `cargo test` asserted *which documents* a clustered search returns: every other test in
   `persisted.rs` asserts round trips, byte spans or hit counts, and the only assertion about
   ranking lives in `scripts/recall.sh`, which runs **outside** the suite so a sweep does not
   rebuild 20,000 vectors per mutant. `cargo mutants` runs the suite, so it could not see any
   of it. `a_clustered_query_returns_the_documents_it_should` closes that with brute-force
   ground truth over a few hundred vectors, and it kills the mutant — the posting-list byte
   range `first * code_len` turned into `first + code_len`.
   ⚠️ **Two are measurably inert on any fixture the suite can afford**, and are recorded rather
   than chased: the `home` loop's `first..first + len`, which decides the centroid each
   residual is coded against, and `rung1`'s buffer bound `row < first + len`. Mutating the
   first makes every row code against the **origin** instead of its centroid, and in-suite
   recall@10 is **0.9870 either way** — measured, not assumed. Residual coding earns its
   accuracy on hard corpora, and the fixture the suite can afford is seven tight groups in
   twelve dimensions. Catching them needs the gate-scale corpus `recall.sh` runs, and that is
   outside the suite deliberately.
   `a_clustered_index_keeps_its_recall_in_suite` is kept anyway: it asserts ranking quality
   the suite never asserted, with a floor well under the measurement so it fails on a defect
   rather than on drift.
   ⚠️ The remaining two — the `dim > 0` guard widened on a `usize`, and the sibling of the byte
   range — are **not** claimed as analysed here.

## What is not built, and named rather than omitted

- ⚠️ **Nothing is turned on.** The engine still defaults to `replicas: 0`, now as a *measured*
  default rather than a correctness clamp: at `Query::default()`'s `p=8` replication buys
  0.0000 recall on clustered data and costs bytes, and on uniform data it matches probing
  wider while costing 1.99× the stored codes. The ~33% query-byte saving is reachable only at
  small `p`, and **lowering `p` is a separate decision with its own measurement**. Until it is
  taken, this milestone has fixed a real defect and enabled an option nobody exercises.
- ⚠️ **The mapping's position in the body is load-bearing**, which
  `a_rung_zero_query_reads_no_int8_or_float_bytes` caught: sections lie in attachment order and
  the coalescer merges nearby ranges, so with the mapping after `Sq8` a rung-0 fetch bridged
  straight across it and dragged in **44,784** int8 bytes rung 0 exists to avoid. It sits
  between `RaBitQ` and `Sq8`. Nothing gates that ordering — a future section inserted between
  them would reintroduce it.
- **Replication for sparse or text retrieval.** This is the dense index's mechanism.
- **Rebuilding existing segments.** Absent mapping means index rows are data rows, so every
  segment already written stays correct and unreplicated.
