# Dense, Sparse, and Full-Text: One Data Model, Three Shipping Dates

**Answers:** Q39 — *How do we support dense and sparse vectors now and full-text later,
without a rewrite?*
**Status:** Complete (v1)

## 1. The actual risk

Shipping dense-only is the right *sequencing* decision and the wrong *modelling* decision if
done naively. The trap is concrete:

> A schema of "one document has one vector" and a segment format with a single vector section
> cannot absorb sparse vectors, named vectors, multi-vector, or BM25 without a migration of
> every customer's data.

At 50M indexes, a format migration is not an inconvenience — it is a year. So the discipline
is: **build the general data model on day one; implement one retriever at a time.**

## 2. The three modalities are one structure

| Modality | Structure | Payload per posting |
|---|---|---|
| **Dense ANN** | Clustered posting lists (SPANN), keyed by centroid | quantized code |
| **Sparse / learned sparse** (SPLADE, miniCOIL) | **Inverted index**, keyed by term/dimension | float impact |
| **BM25 full-text** | **Inverted index**, keyed by term | term frequency + fieldnorm |

Sparse and BM25 are the *same machinery* — an inverted index with a per-posting weight — and
Qdrant confirms this operationally: sparse vectors there are *"organized in an inverted index,
a separate data structure from HNSW"*, and search over them is **exact**, not approximate.

> **D-72 (restates and strengthens D-15).** The posting-list format carries a **generic impact
> payload** from v1: `(doc_id, impact)` where impact is a configurable u8/f16/varint. BM25 term
> frequencies, SPLADE weights, and miniCOIL weights are all just impacts. Building this once
> means full-text is a *scorer plus a tokenizer*, not a new storage engine.

This is the single most important sequencing decision in the document.

## 3. The data model to commit to now

```jsonc
// Schema — the shape must exist in v1 even if only `dense` is implemented
{
  "fields": {
    "title":  { "type": "text", "index": ["bm25", "trigram"] },        // later
    "body":   { "type": "text", "index": ["bm25"] },                   // later
    "tags":   { "type": "string[]", "index": ["filter"] },
    "created_at": { "type": "datetime", "index": ["filter", "sort"] }
  },
  "vectors": {                                    // NAMED, plural, from day one
    "body_dense":  { "kind": "dense",  "dims": 768, "metric": "cosine" },
    "body_sparse": { "kind": "sparse", "model": "splade" },            // later
    "body_late":   { "kind": "multi",  "dims": 128, "scoring": "maxsim" } // later
  }
}
```

Three properties that must be present in v1 regardless of what is implemented:

1. **Vectors are named and plural.** Qdrant's *named vectors* — *"multiple vectors for the same
   point, for example one dense and one sparse"* — is the right model and is what hybrid
   prefetch-then-fuse needs. A singular `vector` field is the migration trap.
2. **A document may own many vectors** (multi-vector / late interaction). Retrofitting
   "documents have *n* vectors" into a one-row-one-vector layout is a rewrite (D-28).
3. **Segment sections are optional and self-describing.** The footer names which sections
   exist; a v1 segment simply has no `sparse_postings` or `positions` section. Readers skip
   what they do not find. No version bump, no migration — old segments stay valid forever
   because they are immutable.

## 4. Query API shaped for all three now

The `query` request from [`../11-design/api-design.md`](../11-design/api-design.md) already has
the right shape; making it explicitly multi-retriever costs nothing today:

```jsonc
{
  "prefetch": [
    { "vector": { "field": "body_dense",  "query": [...] }, "limit": 200 },
    { "sparse": { "field": "body_sparse", "query": {...} }, "limit": 200 },   // later
    { "text":   { "field": "body", "query": "quarterly revenue" }, "limit": 200 } // later
  ],
  "fusion": { "method": "rrf", "k": 60 },
  "filter": [...],
  "rerank": "fast",
  "top_k": 20
}
```

Qdrant's Query API uses exactly this prefetch-then-fuse shape. A v1 that accepts a single
`prefetch` entry and rejects the others is forward-compatible; a v1 with a flat
`{"vector": [...]}` is not.

> **D-73.** Ship the `prefetch[]` + `fusion` request shape in v1 even though only one retriever
> exists. The API is the hardest thing to change later — harder than the format, because
> customers' code depends on it.

## 5. Sparse-specific notes

- **Sparse search is exact, not approximate.** No recall knob, no ANN structure — which means
  it is a *simpler* subsystem than dense, and cheap to add once postings exist.
- **SPLADE vs miniCOIL** is a real product choice: SPLADE *"adds related terms the text never
  used, recovering synonyms"*, giving higher recall; miniCOIL *"keeps BM25's term matching but
  reweights each term by context"*, producing a **smaller, more compact index** with higher
  precision and lower recall. miniCOIL also *"fully reuses the outputs of dense encoders"*,
  making it a cheap upgrade path.
- We should not *embed* either model (D-29: no inference in `pstore`). We accept sparse vectors
  as input and store/search them. That keeps the choice with the customer and us out of the
  model-lifecycle business.
- Impact quantization matters for R: u8 impacts vs f32 is 4× on the posting payload.

## 6. Full-text later, safely

FTS is deferred to M5, and the deferral is safe if and only if these exist beforehand:

| Prerequisite | Where | Why |
|---|---|---|
| Generic impact payload in postings | D-72 | BM25 is postings + a scorer |
| Optional, self-describing segment sections | `file-format-and-layout.md` | Add `positions`, `term_dict` without a migration |
| `text` field type in the schema, even if unimplemented | §3 | Customers declare intent; we reject with a clear "not yet" |
| `prefetch[]` + `fusion` in the API | D-73 | No breaking change when the third retriever arrives |
| Block-max metadata in the **index section**, not interleaved | D-13 | Skipping a block must skip a *network fetch* |
| Per-segment DF summaries fetched in RT-A | D-30 | Cross-shard BM25 needs two-pass IDF; retrofitting is a correctness bug hunt |

The last two are the ones that would genuinely hurt to retrofit, because they are *layout*
decisions inside the segment, not additive sections.

> **D-74.** Reserve the section identifiers and the schema vocabulary for BM25, positions,
> trigram, and sparse in v1. Reserving a name costs nothing; needing a format version bump
> across 50M indexes costs a year.

## 7. Sequencing

| Phase | Modality | Effort | Rationale |
|---|---|---|---|
| **M3** | Dense ANN (SPANN + RaBitQ) | high | The hard part; sets the format and the round-trip budget |
| **M5a** | **Sparse vectors** | **low** | Exact search over postings we already have. Should land *before* BM25 — it is cheaper and unlocks hybrid immediately. |
| **M5b** | BM25 full-text (Tantivy behind our `Directory`) | medium | Tokenizers, analyzers, query parser, two-pass IDF |
| **M6** | Multi-vector / late interaction | medium | Storage multiplies ×10–100; needs 1-bit quantization to be affordable |
| later | Trigram regex/glob | low | Same inverted machinery |

> **Revision to the roadmap.** [`../11-design/roadmap.md`](../11-design/roadmap.md) has M5 as
> "full-text and hybrid". **Split it: sparse first (M5a), BM25 second (M5b).** Sparse gives us
> a genuine hybrid story for a fraction of the work, and it exercises the fusion path with a
> retriever whose correctness is easy to verify (exact search) before layering on BM25's
> scoring subtleties.

## 8. What we deliberately do not model yet

- Cross-modal (image/text) — the schema handles it as another named dense vector; no special
  work.
- Model inference and reranking (D-29) — a different product.
- Matryoshka / truncated embeddings — a `dims` variant of a dense field; handled by the schema
  without new structure, but the recall implications at 1-bit are open (OQ-41).

## 9. Open questions raised

- OQ-126 — Impact payload encoding: u8 vs f16 vs varint, and its effect on posting-list size
  and therefore on R.
- OQ-127 — Should sparse postings live in the *same* inverted index as BM25 terms (one term
  space) or a parallel one? Sharing simplifies fusion and layout; separating avoids term-id
  collisions between an analyzer's vocabulary and a model's.
- OQ-128 — Multi-vector storage: interleaved per document, or a separate section (refines
  OQ-65)? Interacts with the MaxSim scan pattern and therefore with cache locality.
- OQ-129 — Does accepting sparse vectors as *input* (rather than embedding text ourselves)
  match how customers actually work, or does it push too much of the pipeline onto them?
  Worth asking design partners.

## Sources

- [Sparse Vectors and Inverted Indexes — Qdrant](https://qdrant.tech/course/essentials/day-3/sparse-vectors/)
- [miniCOIL: on the Road to Usable Sparse Neural Retrieval — Qdrant](https://qdrant.tech/articles/minicoil/)
- [Hybrid Search with Qdrant's Query API (prefetch + fusion, named vectors)](https://qdrant.tech/articles/hybrid-search/)
- [A guide to hybrid search (using SPLADE) in Qdrant — Viraj Kadam](https://viraajkadam.medium.com/a-guide-to-hybrid-search-using-splade-qdrant-vector-database-a4b70e243f4a)
- [Inverted Index Storage — tantivy (DeepWiki)](https://deepwiki.com/quickwit-oss/tantivy/8.3-term-dictionary-and-posting-lists)
- [turbopuffer — Concepts (schema, BM25, trigram indexes)](https://turbopuffer.com/docs/concepts)
