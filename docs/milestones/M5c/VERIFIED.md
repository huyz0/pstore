# M5c — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

1. **A text field round-trips** — `a_text_field_round_trips`
   (`cargo test -p pstore-format --test text`): the same terms, **exact** term frequencies and
   **exact** fieldnorms, checked per document against the analyzer's own output.
   `the_analyzer_lowercases_and_splits_on_non_alphanumeric` pins the analysis;
   `the_dictionary_carries_document_frequency` pins `df` as **documents**, not postings — the
   two are equal for every term appearing once per document, which is most of them, and differ
   for exactly the common terms IDF exists to discount.
2. **A document with no text does not shift the rows** —
   `a_document_without_text_does_not_shift_the_rows`: an untexted row still occupies a row and
   gets a zero fieldnorm. Skipping it moves every later row by one, and every hit afterwards
   names the wrong document with scores that are internally consistent.
3. **Dense and sparse queries are unaffected by a text field beside them** —
   `a_dense_and_a_sparse_query_are_unaffected_by_a_text_field`
   (`cargo test -p pstore-query --test hybrid`), against the same corpus without one.
   ⚠️ **Observed failing on its first run**, for a fixture reason worth keeping: three indexes
   assembled separately over the input order point at three different documents, because the
   dense clustering decides the row order. There is one builder now (`vec_index::build_all`).
   `a_text_segment_carries_its_postings_and_norms_but_no_field_row` pins the other half — a
   text field has **no `Fields` row**, because a row would hand its postings to `decode_field`
   as `f32`.
4. **BM25 equals an independent implementation** — `bm25_matches_an_independent_implementation`
   (`cargo test -p pstore-index --test text`): top-10 order **and** scores within `1e-4` over
   200 queries on 2,000 documents, against an oracle written from the formula that shares no
   code with the index. ⚠️ The oracle's first version was `O(N²)` — df recomputed per document
   — and took **119 s**; hoisting it made the test 5.25 s. A slow oracle gets deleted, and a
   deleted oracle is how a scorer stops being checked against anything.
   ⚠️ **Its corpus had uniform document lengths until after M5.** BM25's norm is
   `k1(1 − b + b·len/avgdl)`, so when every document is the same length that bracket is
   exactly **1.0** — and `k1 * 1.0` equals `k1 / 1.0`. The entire length-normalisation half of
   the formula, and `b` with it, was invisible to a 200-query oracle comparison. Found by the
   incremental mutation path in six minutes; lengths now vary 0.5×–1.5×.
5. **Global IDF changes the top result** — `global_idf_changes_the_top_result`. On the pinned
   fixture (`common` in 900 of segment A's 1,000 documents and 10 of segment B's, `rare` in 200
   of B's), scoring B with B's own frequencies returns a **different top-1** from scoring it
   with both segments', and the global answer is the one an oracle over the union gives. The
   test also asserts the *local* answer is wrong, so a fixture that happened to agree would
   fail rather than pass silently.
6. **Global statistics cost no round trip** — `./scripts/depth.sh` on the 20,000-document
   gate corpus, and `a_text_query_costs_two_round_trips_beyond_head` as a shape in the suite:
   the dictionary rides beside the footer (`depth == 1` to open) and the postings ride with the
   fieldnorms (`depth == 1` to search). ⚠️ **Moved out of `cargo test` after M5** — it was the
   slowest binary in the workspace at 36 s, which a mutation sweep pays 462 times.
   `statistics_are_a_property_of_the_corpus_not_of_a_query` pins that a summary comes from the
   dictionary alone and that merging is addition.
7. **Bytes are ≤1.2× the query's own lists**, scoped to the `TextPostings` span via
   `Accounted::bytes_in`, and `List == 0` — `./scripts/depth.sh` at gate scale (**2,024 bytes
   moved against 2,024**) and `a_text_query_fetches_only_its_own_lists` as a shape, at the
   pinned `coalesce_gap = 256`.
8. **An unknown term costs nothing** — `an_unknown_term_costs_nothing`: an absent term does not
   change the answer, and a query of only absent terms returns empty with the read counter
   unchanged. `a_segment_with_no_text_is_not_a_text_index` pins the other direction.
9. **A text field survives a compaction, and the dictionary scales with terms** —
   `a_text_field_survives_a_compaction` (`cargo test -p pstore-engine --test sparse`), which
   checks the **merged segment's own postings**: `df` for a term every document uses is 24
   after two 12-document segments merge, so the fold re-analyzed rather than copied.
   `the_term_dictionary_scales_with_terms_not_documents`: 100× the documents over the same
   vocabulary leaves the dictionary byte-identical.
   `a_reaped_text_segment_takes_its_dictionary_with_it` pins the sidecar's reaping.
10. **The ranking gate can fail** — `./scripts/ndcg.sh`:

    | ranker | NDCG@10 | MRR |
    |---|---|---|
    | BM25 | **1.0000** | **1.0000** |
    | control, IDF removed | 0.5441 | 0.8667 |
    | floor | 0.80 | 0.75 |

    ⚠️ **BM25's 1.0000 is not a quality claim.** Relevance is *planted* — each query owns a
    marker term in exactly 5 documents — so a scorer that weights by IDF finds them trivially.
    That is the point: what the gate demonstrates is that the corpus **discriminates**, because
    the control with IDF removed falls to 0.5441 and misses the floor. Absolute quality is
    criterion 4's job. MS MARCO is `NOT-RUN`, blocked on data rather than effort.
11. **Three legs, one open, no deeper than the deepest** —
    `a_three_leg_query_is_no_deeper_than_its_deepest_leg`: 2 rounds for dense + sparse + text,
    the same as the text leg alone. `a_text_leg_is_no_longer_refused_and_still_names_what_is_missing`
    pins that `Prefetch::Text` runs with its `query` still a `String`, analyzed at query time.
12. **Gates** — `./scripts/gates.sh` green (8/8), `./scripts/ndcg.sh` green,
    `./scripts/coverage.sh --fail-under-regions 95`, `cargo deny check` clean, mutation below.

## OQ-64, answered with a number

`oq64_how_often_per_segment_idf_is_wrong` — over 37 queries on the two-segment fixture,
per-segment IDF returns a **different top-1 from global IDF on 27 of them (73%)**.

⚠️ `hybrid-and-ranking.md`'s option (2) argues per-shard IDF is acceptable because hash-by-id
sharding makes shards statistically similar. This measures the other case, deliberately: the
fixture's two segments are *unlike each other*. A first attempt put a very common third term in
every query and measured **0 of 100**, which is the same finding from the other side —
per-segment IDF is wrong exactly when the query's **discriminating** term is the one whose
frequency differs between segments.

## What is not built, and one of them is structural

- **Block-max pruning (OQ-45).** D-13 calls it "the single most important FTS layout decision
  for object storage" and `modalities-and-sequencing.md` §6 lists it as a **prerequisite** for
  deferring FTS safely. Segments are immutable, so every segment M5c writes is permanently
  unprunable, and the retrofit costs a new section id plus a compaction pass over the corpus.
  Recorded in **C-11** rather than left as an open question.
- **A bound on the fieldnorms read.** The whole `Fieldnorms` section is fetched on every text
  query — 4 bytes × every row, 80 KB at gate scale against a few KB of postings — and
  criterion 7's byte bound is scoped to `TextPostings`, so this sits outside it. Deliberate:
  fetching only the candidates' norms is a data-dependent second round the budget forbids.
  Bounding it needs a fieldnorm block index, which is not built.
- **A multi-segment caller.** `pstore_query::query` takes one key and `Hit.row` is a
  within-segment row. What M5c builds is D-30's *statistics* half — `TextIndex::search` takes
  global `(df, doc_count)` rather than computing them — and `Stats::merge` is the whole of the
  gathering. The caller needs a stable cross-segment identity, which M5a, M5b and M5c have all
  handed forward by name.
- **A query parser, analyzers, phrase queries, positions, trigram.** `Prefetch::Trigram` is in
  the shape and refused by name, which is what keeps D-73's mechanism tested.
- **A schema.** `DEFAULT_TEXT_FIELD = "text"` is a placeholder, exactly as `DEFAULT_FIELD` was
  for named vector fields, and it is replaced by the same thing: a schema in HEAD (M6).
