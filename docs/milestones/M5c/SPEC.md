# M5c — BM25, and the IDF that is wrong per segment

**Serves:** **D-15/D-72** (BM25 is postings plus a scorer), **D-30** (two-pass IDF from
per-segment DF summaries fetched in RT-A), **D-31** (ranking quality is a regression suite).
Evidence on **OQ-64** (ranking error from per-shard IDF) and **OQ-127** (one term space, or two).

**Depends on:** [M5a](../M5a/SPEC.md) for the posting codec and the sidecar pattern, and
[M5b](../M5b/SPEC.md) for the leg that makes this a third retriever rather than a second system.

## ⚠️ Not Tantivy — and this banner is an argument, not a measurement

D-14 says start with Tantivy behind a custom `Directory`, and **revisit only "if the
`Directory` indirection costs us round trips we can't recover"**. D-14's premise was that the
alternative is "months of work and a large correctness surface". M5a removed most of it: the
posting codec, the impact payload, the dictionary sidecar, the section plumbing and the
compaction round trip exist and are gated. What is left is what D-72 says is left — *"a scorer
plus a tokenizer"*.

⚠️ **Every other correction banner in this repository is labelled *measured*.** This one cannot
be: measuring `Directory`-over-`BlobStore` request amplification (OQ-43) means building the
thing D-14 recommends, which is the work being declined. So **C-11 is recorded as an
argument**, and what would overturn it is stated: a `Directory` implementation whose cold
multi-term query stays inside three sequential round trips.

**What is given up, and the last one is structural.** No query parser, no language analyzers,
no phrase queries, no positions, no regex — all *deferrable*, addable to later segments without
touching what shipped. **Block-max metadata is not.** D-13 calls it "the single most important
FTS layout decision for object storage" and `modalities-and-sequencing.md` §6 lists it as a
**prerequisite** for deferring FTS safely, in the group that "would genuinely hurt to retrofit".
Segments are immutable, so **every segment M5c writes is permanently unprunable**, and the
retrofit costs a new section id plus a compaction pass over the whole corpus. Shipping against
a stated prerequisite, said out loud.

## ⚠️ Three decisions the corpus does not make

**A text field is the attribute named `text`.** No new `Document` shape, and no `Fields` row:
attributes already round-trip through `Segment::scan`, which is what lets a compaction
**re-analyze the original string** — postings cannot be inverted back into text, because order,
duplicates and dropped tokens are gone. `DEFAULT_TEXT_FIELD = "text"` is a placeholder for a
schema exactly as `DEFAULT_FIELD = "vector"` was the placeholder for named vector fields, and
it is replaced by the same thing: a schema in HEAD, which is M6's catalog.

⚠️ **A consequence worth stating**, because the first draft of this spec had it wrong: a text
field needs **no `Fields` row and no `kind: 2`**. The `Fields` table describes *vector* fields,
whose layout a reader must know to decode them; a text field's presence is answered by whether
its section exists. Adding a row would have put postings in front of `decode_field`, which
would have read them as `f32` and returned a dense field of noise — the failure M5a's criterion
4 exists for, reintroduced by an unnecessary table entry.

**Two term spaces, not one — OQ-127 answered.** A sparse dimension is a `u32` a model emits; a
BM25 term is a string an analyzer produced. Sharing a space lets an analyzer's term id and a
model's dimension id collide, and a collision does not fail — it **adds** two unrelated signals
into one list. Separate sections, separate sidecars, separate `Fields` rows.

**Single-segment, like M5a and M5b.** `pstore_query::query` takes one key and `Hit.row` is a
within-segment row; M5a and M5b both handed a stable cross-segment identity forward to
full-text **by name**. M5c does not build it either: it builds the **statistics** half of D-30
— `TextIndex::search` takes global `(df, doc_count)` rather than computing them from its own
segment — and criterion 5 proves that is not decoration. The caller that gathers them across
segments is the milestone after this one, and it is named rather than implied.

## The numbers this milestone is pinned to

| Name | Value | Why this value |
|---|---|---|
| BM25 `k1`, `b` | **1.2**, **0.75** | Lucene/Robertson defaults. Exposed, not tuned: tuning without an eval set is fitting to a fixture. |
| Oracle agreement | within **1e-4** of an independent in-memory BM25 over the same corpus | ⚠️ The real gate. A judged set we wrote grades our own homework; a second implementation of the formula does not. |
| Fieldnorm | **exact `u32` token count**, one per row, in its own section | ⚠️ *Not* Lucene's lossy byte. Lossy would make criterion 1 unsatisfiable as "the same fieldnorm" and put a second error term between us and the oracle, for 3 bytes a row. |
| Text exactness corpus | **2,000 documents**, 3,000-term Zipf vocabulary, 40 terms each; **200 queries** of 3 terms drawn from a random document | Small enough for a brute-force oracle in the test budget; skewed because a uniform vocabulary makes every IDF the same and hides the term that matters. |
| Gate corpus | **20,000 documents**, ~120 terms each, same vocabulary shape | ~2.4M postings, ~9 MB of section: large enough that a whole-section read is visible. |
| `coalesce_gap` for the byte and round criteria | **256 bytes** | ⚠️ Pinned because it decides the result. The defaults are 64 KiB and 1 MiB; on a 9 MB section either merges most of it, and the whole-section read the bound exists to refuse becomes the measured behaviour. |
| Two-pass fixture | **2 segments**, 1,000 docs each; one term in **900** docs of segment A and **10** of segment B | A ratio that moves IDF by more than a factor of 4, so the disagreement reaches the **top-1**, not only the score. |
| Judged set | **60 queries** over the exactness corpus, judgments from generation; **NDCG@10 ≥ 0.80**, **MRR ≥ 0.75** | A regression floor, **not** a quality claim — and gated by a control (criterion 10) so it cannot pass everything. |
| Analyzer | lowercase, split on non-alphanumeric; **no stemming, no stopwords** | Each is a quality knob, and a knob chosen without an eval set is noise. Named, not shipped. |

## Delta

**Adds**
- `Section::TextPostings` (id **12**) and `Section::Fieldnorms` (id **13**) — reserved here,
  never reused. `Section::TermDict` (id 6) stays reserved and **stays unused**: the dictionary
  is a sidecar, `<segment>.tdict`, for the reason C-10 gives.
- `pstore_format::text` — the analyzer, postings keyed by `field\0term` whose impact is the
  term frequency, and a dictionary carrying each term's **document frequency**. ⚠️ Built over
  documents **in segment row order**, the trap `sparse::build` already carries: a clustered
  dense field reorders rows, and a fieldnorm taken from the wrong row is a wrong score.
- `ImpactEncoding::Varint` — **exact** `u32` impacts, which is what a term frequency is, and
  the third of the three encodings D-72 names ("u8/f16/**varint**"). ⚠️ `U8`'s per-term scale
  is a *quantization*, and a quantized tf puts a second error term between the scorer and the
  oracle criterion 4 measures against.
- `Segment::has_text()`, answered by whether the section exists — **no `Fields` row**, for the
  reason above.
- `pstore_index::text::TextIndex` — `open`, and `search(store, key, terms, stats, k)` where
  `stats` is **global** `(df per term, doc_count)`, supplied by the caller.
- `pstore_query::Prefetch::Text` stops being refused. ⚠️ Its `query` **stays a `String`** and
  the analyzer runs at query time — D-73's premise is that the request shape is the hardest
  thing to change, so this milestone does not change it.
- `scripts/ndcg.sh`, wired into `.github/workflows/ci.yml` **and** AGENTS.md's Gates table —
  `scripts/build-index.py --check` asserts set equality between them, so a script added to one
  and not the other is a red tree.
- **C-11** on `full-text-search.md` (D-14) and its `INDEX.md` row.

**Does not add** — a query parser (`AND`/`OR` is `fusion`'s job); phrase queries and positions
(id 7 stays reserved); trigram; stemming, stopwords, language analyzers; block-max pruning
(OQ-45), whose absence is now a stated cost rather than a deferral; **cross-segment fusion and
the multi-segment caller**; **MS MARCO** — no network and no dataset here, so the roadmap's
MS MARCO evaluation is `NOT-RUN`, blocked on data exactly as M3's 100M-vector scale is.

## Acceptance criteria

1. A text field round-trips: the same terms, the same **exact** term frequencies, the same
   **exact** fieldnorms.
2. A document with no `text` attribute, or one that is not a string, contributes no postings
   **and does not shift row numbering** — postings address segment rows, and a skipped row
   moves every later one by exactly the amount nothing would notice.
3. **Dense and sparse queries are unaffected by a text field sharing the segment** — the same
   rows and scores as without one.
4. Scores equal an independent in-memory BM25 to within **1e-4**, over the 200 pinned queries,
   at `k1 = 1.2`, `b = 0.75`.
5. **Two-pass IDF changes the top-1, and the global answer is the right one.** On the pinned
   two-segment fixture, per-segment IDF and global IDF return **different top-1 rows**, and the
   global one matches the oracle over the union. Evidence for OQ-64.
6. Gathering global DF costs **no extra round**: every segment's dictionary arrives in the open
   round, so a text query is **2 rounds beyond the open** for any number of segments.
7. Bytes fetched inside the `TextPostings` span are ≤ **1.2×** the query's own posting lists,
   at the pinned `coalesce_gap`; `List == 0`.
8. A term absent from the vocabulary adds no request and no bytes, and is not an error.
9. A text field survives `Engine::write` → fold → **compaction** intact, **and** the sidecar
   scales with distinct terms, not documents.
10. `NDCG@10 ≥ 0.80` and `MRR ≥ 0.75` on the judged set by `scripts/ndcg.sh` — **and the same
    script scores a deliberately degraded ranker and fails if that clears the floor too.**
    ⚠️ Without the control the gate cannot fail: judgments derived from generation are ranked
    correctly by any term-matching scorer, including one with `k1` and `b` transposed.
11. A three-leg hybrid (dense + sparse + text) is **no deeper than its deepest leg** and opens
    the segment once.
12. Region coverage ≥95% on shipped crates, mutation ≥80% on the new modules, gates green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `a_text_field_round_trips` | a fieldnorm taken from the wrong row; term frequency counted as 1; a varint impact truncated to u8 |
| 2 | `a_document_without_text_does_not_shift_the_rows` | a row skipped rather than given an empty norm |
| 3 | `a_dense_query_is_unaffected_by_a_text_field`, `a_sparse_query_is_unaffected_by_a_text_field` | a `Fields` row for text, which sends postings to `decode_field` as f32 |
| 4 | `bm25_matches_an_independent_implementation` | `k1`/`b` transposed; the `+1` dropped from the IDF logarithm; the norm applied unnormalised |
| 5 | `global_idf_changes_the_top_result` | DF summed over the queried segment only — the same answer on any one-segment fixture |
| 6 | `a_text_query_costs_two_round_trips_beyond_head` | the dictionary awaited after the footer instead of beside it |
| 7 | `a_text_query_fetches_only_its_own_lists` | reading the whole section and filtering in memory |
| 8 | `an_unknown_term_costs_nothing` | an error for a term with no postings |
| 9 | `a_text_field_survives_a_compaction`, `the_term_dictionary_scales_with_terms_not_documents` | `scan` returning documents without their text; a dictionary entry per document |
| 10 | `./scripts/ndcg.sh` | a floor moved to whatever was measured; a gate that passes a degraded ranker |
| 11 | `a_three_leg_query_is_no_deeper_than_its_deepest_leg` | a leg awaited in sequence |

⚠️ **"Beyond the open", not "from HEAD".** `pstore_query::query` never reads HEAD, so the tests
measure the rounds this crate issues, exactly as M5a's renamed depth test does.

## RA budget

*t* is the query's terms with a dictionary hit; *s* is the segments a caller opens.

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| Seal a segment with a text field | +1 PUT (term dictionary sidecar) | — | — | 0 |
| Text query, cold, one segment | 0 | **2 beyond the open** — {footer, dictionary} ∥ {postings, fieldnorms} | 2 + *t* + 1 | 0 |
| Text query, cold, *s* segments | 0 | **2** — every dictionary in the same round, which is what makes two-pass free | 2*s* + (*t* + 1)·*s* | 0 |
| Three-leg hybrid, cold | 0 | **2** | 4 + *t* + 1 + *p* (+ *p*) | 0 |

## Risks

- **The judged set is ours.** Criterion 4 (agreement with an independent implementation) is the
  real gate; criterion 10 is a regression floor with a control that makes it able to fail.
- **No analyzer means no recall on real text.** Lowercasing and splitting is not stemming.
  Changing analysis requires reindexing, so it is a decision, not a default.
- **Byte cost is unbounded in the query's own terms**, and without stopwords a common term's
  list is a large fraction of the section. This is where OQ-45 stops being optional; criterion
  7 measures the over-read, not the appetite.
- **The dictionary grows with the vocabulary**, and English prose has a long tail. Criterion 9
  measures the scaling; nothing caps it.

## Tasks

| Id | Commit |
|---|---|
| **M5c.1** | `pstore_format::text`: the analyzer, the varint impact, the term dictionary with its DF, the fieldnorms section, and the fold and compaction that carry them |
| **M5c.2** | `TextIndex` and BM25 against an independent oracle, one segment, with depth and bytes |
| **M5c.3** | Two-pass IDF: global statistics, the pinned two-segment fixture, and OQ-64's number |
| **M5c.4** | The third leg in `pstore-query`, `scripts/ndcg.sh` with its control, CI and Gates wiring, and C-11 |
