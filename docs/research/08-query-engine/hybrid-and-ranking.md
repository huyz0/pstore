# Hybrid Search and Ranking

**Answers:** Q27
**Status:** Complete (v1)

## Why hybrid is the product

Dense retrieval misses exact matches (identifiers, rare terms, code symbols, names). Sparse
retrieval misses paraphrase and semantics. Every serious RAG system runs both. The engines
that own this — Weaviate is cited as best-in-class — win on capability, and the ones with
Tier-3 economics (S3 Vectors) don't have it at all. **Hybrid at $70/TB/month is the product
thesis** (see `01-prior-art/vector-search-landscape.md`).

## Fusion: RRF by default, weighted as an option

| Method | Mechanism | When |
|---|---|---|
| **Reciprocal Rank Fusion (RRF)** | Combine by *rank position*: `Σ 1/(k + rank_i)` | **Default.** No score normalization; robust to wildly different score distributions. |
| Weighted score fusion | Normalize scores to a common scale, weighted sum | When one retriever is measurably better and its scores are calibrated. |
| Weighted RRF | RRF with per-retriever weights | Middle ground once you have evidence. |

The decisive argument for RRF as the default is **stability over time**:

> "RRF's stability advantage shows up most when one retriever's score distribution drifts,
> while a score-tuned hybrid setup that beats RRF on the dev set can lose to it on next
> quarter's traffic."

Its cost is that it discards score magnitude, so it cannot exploit a well-calibrated retriever
that knows A is *far* better than B.

> **D-27.** Default: **RRF with `k = 60`** (and expose `k`; note that `k = 10` is reported as
> a common tuned value in current practice — the right value is workload-dependent and must be
> measured). Support weighted RRF and score fusion as explicit options. **Equal weights until
> the customer has measured** — say so in the docs.

## Multi-vector and late interaction

ColBERT-style late interaction and multi-vector documents (Qdrant supports these natively)
are increasingly expected. Structurally: a document owns *n* vectors; scoring is MaxSim over
the query's *m* vectors.

Implications for us:
- Storage multiplies by *n* (~10–100 for ColBERT) — painful, but 1-bit quantization makes it
  survivable.
- Scoring is `m × n` distance computations per candidate — a **compute** problem, not an I/O
  problem, so it fits our architecture (compute happens after the fetch, within a round trip).

> **D-28.** Support multi-vector as a *document-level* concept from the schema up. Retrofitting
> "a document has many vectors" into a one-vector-per-row model is a rewrite. Ship the data
> model even if the MaxSim scorer lands later.

## Sparse vectors

Learned sparse (SPLADE, miniCOIL) shares the posting-list machinery with BM25 given a generic
impact payload (see `06-indexing/full-text-search.md`, D-15). Hybrid then becomes
three-way: dense + lexical + learned-sparse, fused with RRF.

## Reranking

A cross-encoder rerank of the top ~100 is the standard final stage and gives the largest
single quality jump in most RAG pipelines. Pinecone ships built-in inference and reranking.

> **D-29.** **Do not build model inference into `pstore` v1.** It is a different product
> (GPUs, model lifecycle, per-model scaling) and would contaminate a design whose entire
> virtue is having one stateful dependency. Instead: return rich enough results (scores from
> each retriever, the fields a reranker needs, stable ids) that an external reranker is
> trivial, and define a clean extension point for later.

## Scoring correctness across shards and segments

Non-obvious hazard: **BM25 is corpus-statistics-dependent.** IDF computed per segment differs
from IDF over the whole index, so naively merging per-segment top-k gives wrong rankings.

Options:
1. **Global statistics in the manifest** — maintain per-term document frequency at the index
   level, updated at commit. Accurate; adds manifest size and commit work.
2. **Per-shard statistics** — accept approximation; with hash-by-id sharding, shards are
   statistically similar, so error is small.
3. **Two-pass** — gather statistics in RT-A, score in RT-B. Free, since RT-A exists anyway.

> **D-30.** Use **(3) two-pass with per-segment DF summaries fetched in RT-A**, falling back
> to (2) when summaries are unavailable. It costs no extra round trip and is nearly exact.
> This is a correctness issue that is easy to ship wrong and hard to notice.

## Evaluation

Quality must be measured, not asserted:
- A labeled set of **50–200 queries with judged documents**, per corpus type.
- Metrics: **NDCG@10, Recall@k, MRR**, swept over `k` and rank-window size.
- Run it in CI against a fixed corpus so ranking regressions are caught like any other bug.

> **D-31.** Ranking quality gets a **regression test suite**, the same status as correctness
> tests. See `10-benchmarks-cost/evaluation-methodology.md`.

## Open questions raised

- OQ-63: RRF `k` default — 60 (classic) vs 10 (recent practice). Measure on our eval set.
- OQ-64: Cost of global DF maintenance vs. two-pass accuracy. Quantify the ranking error from
  per-shard IDF.
- OQ-65: Multi-vector storage layout — interleaved per document, or a separate section?

## Sources

- [Reciprocal Rank Fusion (RRF): How It Works and When to Use It — BigData Boutique](https://bigdataboutique.com/blog/reciprocal-rank-fusion-how-it-works-and-when-to-use-it)
- [Hybrid Search Explained: Combining Vector and Keyword Retrieval — BigData Boutique](https://bigdataboutique.com/blog/hybrid-search-explained)
- [Introducing reciprocal rank fusion for hybrid search — OpenSearch](https://opensearch.org/blog/introducing-reciprocal-rank-fusion-hybrid-search/)
- [Better RAG Results With Reciprocal Rank Fusion — MongoDB](https://www.mongodb.com/resources/basics/reciprocal-rank-fusion)
- [Hybrid Search with RRF — Chroma Docs](https://docs.trychroma.com/cloud/search-api/hybrid-search)
- [Sifei at SemEval-2026 Task 8: Hybrid Retrieval and Query Rewriting for Multi-Turn RAG — arXiv](https://arxiv.org/pdf/2606.28352)
