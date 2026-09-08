# M5a — Sparse vectors: the postings, and what an impact costs

**Serves:** **D-72/D-15** — one posting format with a generic impact payload. Opens evidence on
**OQ-126** (impact encoding: bytes *and* ranking) and **OQ-127** (one term space, or two).

**Depends on:** [M3](../M3/SPEC.md) (segments, sections, the round-trip budget) and
[M3b](../M3b/SPEC.md), which made `VectorField::Sparse` representable and refused it at the
writer with `"sparse vector fields are not stored yet (M5a)"`. This is that milestone.

## ⚠️ Where the dictionary lives, and why not where the corpus puts it

`full-text-search.md`'s structures table (line 20) puts the **term dictionary** in the "Segment
index section (cached)". That section is bounded by `INDEX_BUDGET` = 8 KiB − footer = **8,150
bytes**, and `try_finish` *refuses* a segment that exceeds it (`writer.rs:180`) rather than
opening it slowly. A SPLADE-sized 30,000-term vocabulary at 12 bytes an entry is 360 KB —
**44× the budget** — so the letter of that row does not make the open slow, it makes the
segment **unwritable**. ⚠️ **Measured at 720,015 bytes, 88×**: the shipped entry is 24 bytes,
not 12, because a list needs its byte length *and* its posting count *and* its scale. The
argument survives its own arithmetic being off by two, which is why it is stated as measured.

So the dictionary is a **sibling immutable object**, `Pinned`, fetched in parallel with the
footer — what M3 does with the centroid table, for the same reason, and a section could not
have a cache class at all. Recorded as **C-10** on that table row. ⚠️ **D-13 is not touched**:
it governs block-max/skip metadata, which this milestone does not add, and `Section::TermDict`
(id 6) stays reserved for full-text's *string* dictionary (OQ-127 stays open).

⚠️ **Its key is derived** — `<segment>.sdict`, never discovered. That is what lets
`Segment::scan` reconstruct a sparse field during compaction; a sidecar the compactor cannot
address turns every merge into silent data loss.

## ⚠️ Which crate owns the codec, against a comment that says otherwise

`writer.rs:26` says "the format does not quantize". That rule is about **vector** quantization
— RaBitQ and int8 are search-index concerns. The impact payload is not: `lib.rs:163` says of
`Impact` that "D-72 makes the impact *encoding* the configurable part… **M5a can change what is
stored here without any caller noticing**". It also has to be the format's: `Engine::fold` and
`Engine::compact` build segments (`engine/src/lib.rs:377`, `:583`) and are **layer 2**, so a
sparse field they cannot write is lost at the first fold. The codec is `pstore_format::sparse`;
`pstore-index` holds only the retriever.

## ⚠️ Exact search, approximate impacts — the tension, named

`modalities-and-sequencing.md` §5: *"sparse search is exact, not approximate."* That is about
**candidate selection** — no probing, no recall knob, every posting of every query term scored.
It is not a promise about score precision, and OQ-126 exists precisely because the payload is
meant to be quantized. Criteria 7, 8 and 13 keep the three claims apart; collapsing them into
"top-k equals brute force" makes the criterion either tautological or unsatisfiable, depending
on which oracle it uses.

## The numbers this milestone is pinned to

Stated here so no criterion can be satisfied by choosing them afterwards.

| Name | Value | Why this value |
|---|---|---|
| Gate vocabulary | **30,000 dimensions** | SPLADE-sized; the size the sidecar deviation exists for. A 50-term fixture would prove nothing about it. |
| Gate corpus | **20,000 rows × 32 non-zero dims**, Zipf-skewed | 640,000 postings, ~2 MB of section. Skewed because a uniform vocabulary hides exactly the hot-list cost. |
| Exactness corpus | **2,000 rows**, 200 queries | Small enough for a brute-force oracle inside the test budget. |
| Impact encoding | **u8, scaled per term** by that term's `max_impact` | D-72's cheapest option; the one to beat, not the one to assume. |
| Impact error bound | ≤ `max_impact / 254` **absolute**, per posting | Fixed by the encoding, asserted, not read back from it. |
| Rank-agreement floor | mean top-10 overlap **≥ 0.95** and top-1 agreement **≥ 0.90** over 200 queries | Chosen before measuring. Below it, the shipped default is f16, not u8. |
| Byte bound, sparse query | bytes fetched **inside the `SparsePostings` span** ≤ **1.2×** the sum of the query terms' posting-list lengths | Catches a whole-section read, which is the failure that still returns the right answer. Scoped to the span because the dictionary and footer are 100× the postings at fixture scale, and unscoped the bound refuses a correct implementation. |
| `coalesce_gap` for criteria 8 and 9 | **256 bytes** | ⚠️ Pinned, because it decides the result. The defaults are 64 KiB (`memory.rs:81`) and 1 MiB (`object_store_backend.rs:49`), and on a 2 MB section either merges most of it — so the whole-section read the criterion exists to catch becomes the measured behaviour. |
| Criterion 13's queries | 200 queries, each the sparse field of a **randomly chosen document** of the exactness corpus | ⚠️ Pinned because it decides the answer: rank agreement under quantization is far more forgiving for uniformly drawn dimensions than for frequency-drawn ones, and the implementation must not get to pick. |

## Delta

**Adds**
- `Section::SparsePostings` (id 5, reserved since M3): per term, row ids as delta varints and
  one impact byte each, doc-ordered. Data area, fetched by range.
- A sparse **dictionary** sidecar — `(dim, offset, len, max_impact)` per distinct dimension,
  sorted by `dim`. One PUT per segment carrying a sparse field: the request class of the M3
  centroid object, which is to say it scales with **bytes**, not documents. Its absence is
  **not an error** — that is how a dense-only segment says "no sparse field here", the same
  way a missing centroid object says "scan me exactly".
- A `Fields` row for sparse fields — `kind: 1`, `vectors: SparsePostings`, `dims: 0`. ⚠️
  `seal_segment` today skips any field whose `dims == 0` (`writer.rs:244`), and `d.field()`
  returns `&[]` for a sparse field, so un-refusing sparse without this writes a field that
  `Segment::field_layout` cannot see.
- ⚠️ **The field index that assigns legacy section ids counts dense fields only.**
  `seal_segment` gives `fi == 0` the ids `Vectors`/`RaBitQ`/`Sq8` (`writer.rs:224`), and names
  are sorted, so a sparse field named `body_sparse` beside a dense `vector` would take slot 0
  — the dense field would move to `FieldVectors`/`FieldRaBitQ`/`FieldSq8`, ids 2/3/4 would
  never be emitted, and `VecIndex::search` would hit its `else { return Ok(Vec::new()) }`
  (`vec_index.rs:405`) and return **zero rows, silently**. Whether hybrid worked would depend
  on the alphabetical order of two field names.
- `pstore_format::sparse` — the transposition, the impact encodings and the dictionary.
- `pstore_index::sparse::SparseIndex`, and a constructor **from an already-open `Segment`** — so
  [M5b](../M5b/SPEC.md) can run two legs over one open rather than opening the segment twice.
- `Engine::fold` and `Engine::compact` write and re-read a sparse field, and switch from
  `finish()` to `try_finish()` so a segment that would drop one is refused rather than
  written.
- `check_storable` stops refusing sparse — a deletion, and only once the fold can store it.
  `try_finish` refuses in its place, and more narrowly: a sparse document whose
  `SparsePostings` section was **not attached**. ⚠️ The M3b lesson is not that sparse was
  unsupported, it is that a writer accepting what it silently drops is the bug.

**Does not add** — BM25, tokenizers, string terms (M5c); positions; trigram; block-max pruning
(OQ-45: postings here are doc-ordered and, sparse being exact, there is no top-k bound to prune
against yet); **more than one sparse field per segment** — the second needs a `FieldSparse*` id
pair like `FieldVectors`, and one field is what fusion needs; **fusion** and the `prefetch[]` request shape, which are
[M5b](../M5b/SPEC.md). ⚠️ **M3b's carried item** — "the clustered
multi-field case is carried to M5a explicitly" (M3b SPEC:63) — is **re-deferred**, not dropped:
it is a dense-field question, sparse fields are never clustered, and nothing here touches it.

## Acceptance criteria

1. A sparse field round-trips through the codec: the same dimensions, and every impact within
   `max_impact / 254` **absolute** of the value written.
2. A sparse document is stored when its `SparsePostings` section is attached, and **refused**
   when it is not, with a message naming the section.
3. A sparse field has a `Fields` row that `Segment::field_layout` returns, with `kind == 1`.
4. **A dense query is unaffected by a sparse field sharing the segment** — the same rows and
   scores as without one, whatever the two field names sort like.
5. The dictionary's length scales with **distinct dimensions, not documents**: 10× the rows
   over the same vocabulary leaves it byte-identical in length.
6. A sparse field survives `Engine::write` → fold → **compaction** intact. ⚠️ The one that
   fails silently: a compaction merges whatever `scan` returned.
7. **Candidates are exact**: the rows scored equal `{row : row shares a dimension with the
   query}` — set equality, not rank equality — over 200 queries on the exactness corpus.
8. **With f32 impacts the ranking is exact**: top-k equals a brute-force scan's, order and
   scores, over the same 200 queries.
9. A sparse query's depth is **exactly 3 from `HEAD`** (2 from open) on the **gate** corpus —
   equality, because a bound cannot see a leg that skipped the dictionary.
10. A sparse query costs **one round beyond open** whatever the term count: `depth() == 1` for
    the postings fetch at the pinned `coalesce_gap`, and `OpClass::List == 0`.
11. Bytes fetched **inside the `SparsePostings` span** are ≤ **1.2×** the sum of the query
    terms' posting-list lengths, at the pinned `coalesce_gap`.
12. A query dimension absent from the dictionary adds no request and no bytes, and is not an
    error — including a query **every** term of which is absent.
13. **OQ-126, both halves**: section bytes under u8, f16 and f32, and top-10 rank agreement of
    u8 and f16 against f32 over the pinned queries, reported against the pinned floor. The
    shipped default is the cheapest encoding that clears it.
14. Region coverage ≥95% on shipped crates, mutation ≥80% on the new modules, full gate set
    green.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `a_sparse_field_round_trips` | a fixed global scale; `max_impact` dropped from the entry, so every list decodes against the wrong scale |
| 2 | `a_sparse_field_without_its_postings_is_refused` | `try_finish` accepting it, which is M3b's silent drop with a new name |
| 3 | `a_sparse_field_has_a_layout_row` | the `dims == 0 { continue }` skip left in place |
| 4 | `a_sparse_field_does_not_displace_the_dense_one` | `fi` counted over all fields, so a sparse name sorting first takes the legacy section ids |
| 5 | `the_dictionary_scales_with_terms_not_documents` | an entry per document, i.e. a posting list per row |
| 6 | `a_sparse_field_survives_a_compaction` | `scan` returning documents without their sparse field, which merges to nothing |
| 7 | `the_candidate_set_is_exact` | `truncate` on a posting list; dropping the query's last term |
| 8 | `f32_impacts_rank_exactly_like_brute_force` | `min` instead of `+=` in the accumulator; scoring `q·q` |
| 9 | `a_sparse_query_costs_two_round_trips_beyond_head` ⚠️ renamed: the test measures the two rounds that belong to the index, as M3's equivalent does; HEAD is the engine's and is not in this crate | fetching the dictionary after the footer instead of beside it |
| 10 | `the_postings_fetch_is_one_round` | a `get_range` awaited per term in a loop |
| 11 | `a_query_fetches_only_its_own_lists` | reading the whole section and filtering in memory |
| 12 | `an_unknown_dimension_costs_nothing` | `UnknownField` returned; an all-absent query erroring rather than returning nothing |
| 13 | `impact_encodings_are_measured` (bounds asserted, numbers reported) | relaxing the floor to whatever was measured |
| 14 | `./scripts/coverage.sh --fail-under-regions 95`, `cargo mutants` | a new module that runs in tests without being constrained by them |

## RA budget

Measured **from `HEAD`**, as M3 is, because that is what a client experiences, and on the gate
corpus, because M1's depth invariant held at 500 rows and broke at 40,000. *t* is the number of
query dimensions **found in the dictionary**, after coalescing.

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| Seal a segment with a sparse field | +1 PUT (dictionary sidecar) | — | — | 0 |
| Sparse query, cold | 0 | **3** — HEAD (1) ∥ {footer, dictionary} (2) ∥ postings (3) | 1 + 2 + *t* | 0 |
| Warm (dictionary and index section cached) | 0 | **1** | *t* | 0 |

⚠️ `Rpar` **scales with query terms**, not with documents — which is the allowed direction, and
is stated rather than hidden behind "one call": `get_ranges` coalesces but does not merge lists
that are far apart (`store.rs:109`), so a 32-term query is up to 32 requests in one round.

## Risks

- **The impact byte is a silent quality regression** — nothing fails, results merely get a
  little wrong. Criterion 13 makes it a number against a floor fixed in advance; 7 and 8 keep
  exactness separable so a later encoding cannot quietly redefine it.
- **The dictionary is unbounded in the vocabulary.** A distinct dimension per document makes
  the sidecar larger than the postings. Criterion 5 measures the scaling; it does not cap it.
- **Bytes, not depth, is the sparse failure mode.** A 1,000-term query fetches 1,000 lists in
  one round. Criterion 11 bounds over-read, not the query's own appetite; block-max (OQ-45) is
  where that gets bounded, and it is not here.
- **Fusion by row does not survive a second segment**, stated so full-text treats a stable id
  as a task rather than a discovery.

## Tasks

| Id | Commit |
|---|---|
| **M5a.1** | `pstore_format::sparse`: the codec, the section, the `Fields` row, the derived dictionary key, and the refusal that replaces M3b's |
| **M5a.2** | The engine: a sparse field survives write → fold → compaction, and `check_storable` narrows |
| **M5a.3** | `SparseIndex::open`/`search` — exact candidates, exact f32 ranking, depth, requests and bytes |
| **M5a.4** | OQ-126: the encoding harness, the numbers, and the default they select |

⚠️ **Landed as two commits, not four.** M5a.1 and M5a.2 are one property: a format that can
hold a sparse field while the fold cannot write one is a state that exists only to satisfy a
task boundary, and it has to be guarded by a temporary refusal that the next commit deletes.
M5a.3 and M5a.4 are one property too — the measurement selects the default the retriever
ships with, and splitting them means committing a default nothing has measured.
