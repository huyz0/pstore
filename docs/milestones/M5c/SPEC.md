# M5c — BM25, and the IDF that is wrong per segment

**Serves:** **D-15/D-72** (BM25 is postings plus a scorer), **D-30** (two-pass IDF with
per-segment DF summaries fetched in RT-A), **D-31** (ranking quality is a regression suite).
Answers **OQ-127** (one term space, or two).

**Depends on:** [M5a](../M5a/SPEC.md) for the posting codec and the sidecar pattern, and
[M5b](../M5b/SPEC.md) for the leg that makes it a third retriever rather than a second system.

## ⚠️ Not Tantivy, and D-14 said Tantivy

> **D-14.** Start with **Tantivy behind a custom `Directory`**… Revisit a native
> implementation only if the `Directory` indirection costs us round trips we can't recover.

D-14's premise was that the alternative is "months of work and a large correctness surface
(tokenization, scoring, phrase queries, positions)". **M5a removed most of that premise**:
the posting codec, the impact payload, the dictionary sidecar, the section plumbing and the
compaction round trip all exist and are gated. What is left for BM25 is what D-72 says is left
— *"a scorer plus a tokenizer"* — and D-14's own escape clause names the reason to take it:
`Directory` assumes cheap small reads, the doc itself warns *"a naive implementation will
produce request storms"*, and it is a **synchronous** API in front of an async store whose
whole architecture is round-trip depth.

Recorded as **C-11** on D-14. ⚠️ What this gives up is real and named: no query parser, no
language analyzers, no phrase queries, no positions, no regex. Those are what Tantivy would
have brought for free, and every one of them is deferred rather than delivered.

## ⚠️ OQ-127 answered: two term spaces, not one

A sparse dimension is a `u32` a model emits; a BM25 term is a string an analyzer produced.
Sharing one space means an analyzer's term id and a model's dimension id can collide, and a
collision does not fail — it **adds** two unrelated signals into one posting list. Separate
sections, separate sidecars, separate `Fields` rows. The cost is one more section id and one
more sidecar per segment that has text; the alternative is a scoring bug with no symptom.

## The numbers this milestone is pinned to

| Name | Value | Why this value |
|---|---|---|
| BM25 `k1`, `b` | **1.2**, **0.75** | The Lucene/Robertson defaults. Exposed, not tuned here: tuning without an eval set is fitting to a fixture. |
| Oracle agreement | scores within **1e-4** of a textbook in-memory BM25 over the same corpus | ⚠️ The real gate. A judged set we wrote ourselves grades our own homework; an independent reimplementation of the formula does not. |
| Judged set | **60 queries** over a 2,000-document corpus, judgments derived from generation, **NDCG@10 ≥ 0.80** | A regression floor, not a quality claim. Stated as such. |
| Gate corpus | **20,000 documents**, Zipf term frequencies, ~120 terms each | ~2.4M postings; large enough that a whole-section read is visible in the byte bound. |
| Fieldnorm | 1 byte per row, in the sidecar | Lossy exactly as Lucene's is; the oracle uses the **decoded** norm, so the loss is in the formula both sides compute. |
| Analyzer | lowercase, split on non-alphanumeric, **no stemming, no stopwords** | Every one of those is a quality knob, and a knob chosen without an eval set is noise. Named, not shipped. |

## Delta

**Adds**
- `Section::TextPostings` (id 12) and `Section::Fieldnorms` — reserved here, never reused.
  `Section::TermDict` (id 6) stays reserved and stays unused: the dictionary is a **sidecar**,
  `<segment>.tdict`, for the reason C-10 gives.
- `pstore_format::text` — the analyzer, the term dictionary (term string → offset, bytes,
  count, **df**), the per-row fieldnorms, and postings whose impact is the term frequency.
  ⚠️ Same codec as sparse, which is D-72's claim made structural rather than asserted.
- A `Fields` row with `kind: 2` for a text field.
- `pstore_index::text::TextIndex` — `open`, and `search` taking **global** statistics.
- `pstore_query::Prefetch::Text` stops being refused, and `query` gathers per-segment DF in
  the open round before scoring in the next.
- `scripts/ndcg.sh` — the ranking-quality gate (D-31), outside `cargo test` for the reason
  `scripts/recall.sh` is: `cargo mutants` must not rebuild a judged corpus once per mutant.

**Does not add** — a query parser (terms come in as a list, `AND`/`OR` is `fusion`'s job);
phrase queries and positions (`Section::Positions`, id 7, stays reserved); trigram regex;
stemming, stopwords or language analyzers; block-max pruning (OQ-45 — still doc-ordered, and
now it costs something, so it is named as the next thing rather than deferred silently);
**MS MARCO** — there is no network and no dataset in this environment, so the roadmap's MS
MARCO evaluation is `NOT-RUN`, blocked on data exactly as M3's 100M-vector scale is.

## Acceptance criteria

1. A text field round-trips: the same terms, the same term frequencies, the same fieldnorms.
2. Scores equal an independent in-memory BM25 to within **1e-4**, over 200 queries on the
   exactness corpus, with `k1 = 1.2`, `b = 0.75`.
3. **Two-pass IDF changes the answer, and the global answer is the right one.** On a corpus
   deliberately split so a term is common in one segment and rare in another, per-segment IDF
   and global IDF rank differently, and the shipped path matches the single-segment oracle
   over the union.
4. Gathering global DF costs **no extra round trip**: a cold text query's depth is **3 from
   `HEAD`** — the DF summaries arrive with the footers, in the open round.
5. Bytes fetched inside the `TextPostings` span are ≤ **1.2×** the query's own posting lists,
   at the pinned `coalesce_gap`.
6. A term absent from the vocabulary adds no request and no bytes, and is not an error.
7. A text field survives `Engine::write` → fold → **compaction** intact.
8. Text and sparse fields in one segment do not share a term space: a sparse dimension whose
   numeric value equals a text term's id scores **only** its own field.
9. A three-leg hybrid (dense + sparse + text) is still **3 from `HEAD`**, and opens the
   segment once.
10. `NDCG@10 ≥ 0.80` and `MRR ≥ 0.75` on the judged set, by `scripts/ndcg.sh`, which fails
    the build below the floor.
11. Region coverage ≥95% on shipped crates, mutation ≥80% on the new modules, gates green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `a_text_field_round_trips` | fieldnorms taken from the wrong row; term frequency counted as 1 |
| 2 | `bm25_matches_an_independent_implementation` | `k1`/`b` transposed; `+1` dropped from the IDF logarithm; the norm applied unnormalised |
| 3 | `global_idf_ranks_differently_from_per_segment_idf` | DF summed over the *queried* segment only, which is the same answer on a one-segment fixture |
| 4 | `a_text_query_costs_two_round_trips_beyond_head` | the DF gather awaited before the footers instead of beside them |
| 5 | `a_text_query_fetches_only_its_own_lists` | reading the whole section and filtering in memory |
| 6 | `an_unknown_term_costs_nothing` | `UnknownField` for a term with no postings |
| 7 | `a_text_field_survives_a_compaction` | `scan` returning documents without their text field |
| 8 | `a_text_term_and_a_sparse_dimension_do_not_collide` | one shared postings section, which adds both signals |
| 9 | `a_three_leg_query_is_no_deeper_than_its_deepest_leg` | a leg awaited in sequence |
| 10 | `./scripts/ndcg.sh` | a floor moved to whatever was measured |

## RA budget

Measured **from `HEAD`**, as M3, M5a and M5b are. *t* is the query's terms with a dictionary
hit; *s* is the number of segments.

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| Seal a segment with a text field | +1 PUT (term dictionary sidecar) | — | — | 0 |
| Text query, cold, one segment | 0 | **3** — HEAD (1) ∥ {footer, term dictionary} (2) ∥ postings (3) | 1 + 2 + *t* | 0 |
| Text query, cold, *s* segments | 0 | **3** — every segment's dictionary in the same round, which is what makes two-pass IDF free | 1 + 2*s* + *t*·*s* | 0 |
| Three-leg hybrid, cold | 0 | **3** | 1 + 4 + *t* + *p* (+ *p*) | 0 |

## Risks

- **The judged set is ours.** NDCG on a corpus we generated measures the generator as much as
  the ranker. That is why criterion 2 — agreement with an independent implementation of the
  formula — is the real gate and criterion 10 is a regression floor, said plainly.
- **No analyzer means no recall on real text.** Lowercasing and splitting is not stemming, and
  a corpus of English prose will miss `running` against `run`. Named, not fixed: an analyzer
  changes the index and therefore needs reindexing, which is a decision, not a default.
- **Byte cost is unbounded in the query's own terms.** A stopword-heavy query fetches a list
  per term, and without stopwords that list can be a large fraction of the section. This is
  where OQ-45's block-max pruning stops being optional; M5c makes the case rather than closes
  it, and criterion 5 measures the over-read, not the appetite.
- **A dictionary per segment scales the open round with segment count.** *s* segments is
  *2s* requests in the open round — width, not depth, and the allowed direction — but it is
  bytes, and compaction is what bounds *s*.

## Tasks

| Id | Commit |
|---|---|
| **M5c.1** | `pstore_format::text`: the analyzer, the dictionary with its DF, fieldnorms, and the write path through fold and compaction |
| **M5c.2** | `TextIndex` and BM25 against an independent oracle; the two-pass IDF that makes a multi-segment answer right |
| **M5c.3** | The third leg in `pstore-query`, and `scripts/ndcg.sh` as the ranking-quality gate |
