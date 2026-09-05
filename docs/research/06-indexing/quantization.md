# Quantization: Bytes per Vector Is the Real Currency

**Answers:** Q20
**Status:** Complete (v1)

## Why quantization matters more to us than to anyone else

For a RAM-resident engine, quantization saves RAM. For `pstore` it does three things at once:

1. **Shrinks posting lists** ⇒ more vectors per ranged GET ⇒ **fewer bytes per round trip
   for the same recall**. This is the dominant effect.
2. **Shrinks the cache working set** ⇒ higher hit rate on the same NVMe ⇒ more warm queries.
3. **Shrinks the centroid table** ⇒ the hottest object stays memory-resident even at 1M
   centroids.

A 32× compression means a 3 MB posting-list fetch covers 32× more vectors. That is a *latency*
win, not just a cost win.

## The options

| Method | Compression | Recall behaviour | Notes |
|---|---|---|---|
| **float32** | 1× | exact | Baseline. 768d = 3,072 B/vector. |
| **float16 / bfloat16** | 2× | near-lossless | Free win; use as the "full precision" tier. |
| **Scalar quantization (int8)** | **4×** | minimal loss (~75% memory reduction, small recall drop) | Safe, simple, SIMD-friendly. |
| **int4 SQ** | 8× | moderate loss | Useful as a *query-side* precision (see BBQ). |
| **Product Quantization (PQ/OPQ)** | 8–64× | good average, **no theoretical bound** | **Max relative error ~100% on real datasets (MSong, Word2Vec)**; "fails disastrously on some real-world datasets." Requires k-means training per index — bad for millions of tenants. |
| **Binary (1-bit)** | **32×** (≈28× realized) | **exceptionally lossy naively** — needs 10×–100× oversampling to recover recall | Extremely fast (Hamming/popcount). |
| **Better Binary Quantization (BBQ)** | 32× storage | Much better: **store 1-bit, quantize the *query* to int4** — large quality gain at no storage cost | Elasticsearch 8.16 / Lucene. |
| **RaBitQ** | 32× (D bits for D dims) | **Sharp theoretical error bound**; "outperforms PQ and its variants in accuracy-efficiency trade-off by a clear margin" | SIGMOD 2024. Randomized; **no per-dataset training**. |

## Decision

> **D-11.** Default pipeline: **RaBitQ (1-bit, asymmetric int4 query) for the posting-list
> scan tier, int8 scalar quantization for the rerank tier, float16 for exact rerank.**

Rationale, in order of importance:

1. **RaBitQ needs no training.** PQ requires k-means over a sample per index. With millions of
   indexes, per-tenant codebook training is an operational nightmare: it must be scheduled,
   stored, versioned, and re-run as the distribution drifts, and a cold tiny index has no
   sample to train on. RaBitQ's randomized construction is **data-independent** — one global
   rotation, no training, works on an index with 10 documents or 10 billion. *For a
   multi-tenant product this single property outweighs raw accuracy comparisons.*
2. **PQ has no error bound and fails badly on some real data** — up to ~100% max relative
   error. For a general-purpose product accepting arbitrary customer embeddings, unbounded
   worst-case error is unacceptable. RaBitQ's bound is a product guarantee we can state.
3. **Asymmetric precision is nearly free.** Storing 1-bit documents while quantizing the query
   to int4 (BBQ's insight, and RaBitQ's construction supports the same asymmetry) buys a large
   accuracy gain at zero storage cost, because the query is quantized once per query, not once
   per vector.
4. **32× compression is the round-trip lever.** See above.

## The rerank ladder

Recall is recovered by oversampling + reranking at increasing precision. Each rung costs
bytes, not round trips (all fetches within a rung are parallel):

```
Rung 0: score all vectors in the p probed posting lists using 1-bit codes  (in the fetched bytes)
        → keep top (k × oversample), oversample ∈ [4, 32]
Rung 1: rerank with int8 codes                        → 1 Rpar (often same blocks)
        → keep top (k × 2)
Rung 2: exact rerank with float16/32 vectors          → 1 Rpar, only if requested
        → final top k
```

- Rung 0 alone typically suffices at 90–95% recall@10 (turbopuffer's stated target).
- Rungs 1–2 are opt-in per query (`rerank: none | fast | exact`), letting a caller buy
  precision with latency.
- Full-precision vectors live in a **separate segment section** so they are never fetched
  during rung 0 (see `05-storage-engine/file-format-and-layout.md`).

> **D-12.** Expose the rerank ladder in the query API. Recall is a *client-selectable* knob,
> like consistency. This is more honest than a fixed internal recall target and lets one
> engine serve both "cheap RAG retrieval" and "exact nearest neighbour" workloads.

## Storage math (768 dims, 1B vectors)

| Tier | Bytes/vector | Total | Comment |
|---|---|---|---|
| float32 | 3,072 | 3.07 TB | Never stored hot. |
| float16 | 1,536 | 1.54 TB | Exact-rerank section, cold. |
| int8 | 768 | 768 GB | Rerank tier. |
| **RaBitQ 1-bit** | **96** | **96 GB** | **Scan tier — this is what the cache holds.** |

96 GB of scan-tier data for a billion vectors is cacheable on a handful of nodes' NVMe. That
is the number that makes warm queries possible at billion scale, and it is the strongest
single argument for aggressive quantization.

## Implementation notes

- **SIMD is mandatory.** 1-bit distance is Hamming via `popcount` (AVX-512 `VPOPCNTQ`, NEON
  `CNT`); int8 is dot product via `VPDPBUSD`/`SDOT`. Use `simsimd` or hand-written
  `std::arch` intrinsics with runtime dispatch. Expect ≫10× over scalar code.
- **Alignment**: quantized codes stored contiguous and 64-byte aligned within blocks.
- **Metric support**: cosine (normalize at ingest), dot product, L2. RaBitQ is formulated for
  inner product / L2 with a normalization step; document the transforms.
- **Rotation**: a single fixed random orthogonal transform (Randomized Hadamard Transform is
  cheap: O(D log D), no matrix storage) applied at ingest and query. Global, versioned, never
  per-tenant.

## Open questions raised

- OQ-39: Measure RaBitQ vs BBQ vs int8 on our target embedding families (OpenAI, Cohere,
  Voyage, open models) at 768/1024/1536/3072 dims.
- OQ-40: Optimal oversample factor per rung as a function of dimension and target recall.
- OQ-41: Does 1-bit hold up at 3072 dims with Matryoshka-truncated embeddings?
- OQ-42: Should we support per-index opt-in PQ for customers who benchmark it as better on
  their data? Adds training infrastructure; defer.

## Sources

- [RaBitQ: Quantizing High-Dimensional Vectors with a Theoretical Error Bound — arXiv / SIGMOD 2024](https://arxiv.org/abs/2405.12497)
- [RaBitQ — ACM Proceedings of the ACM on Management of Data](https://dl.acm.org/doi/10.1145/3654970)
- [Better Binary Quantization (BBQ) — Elasticsearch Reference](https://www.elastic.co/docs/reference/elasticsearch/mapping-reference/bbq)
- [Elastic BBQ: Better Binary Quantization in Lucene & Elasticsearch](https://www.elastic.co/search-labs/blog/better-binary-quantization-lucene-elasticsearch)
- [Binary Quantization: 40x Faster Vector Search — Qdrant](https://qdrant.tech/articles/binary-quantization/)
- [Quantization — Qdrant docs](https://qdrant.tech/documentation/manage-data/quantization/)
- [Binary quantization in Azure AI Search — Microsoft](https://techcommunity.microsoft.com/blog/azure-ai-foundry-blog/binary-quantization-in-azure-ai-search-optimized-storage-and-faster-search/4221918)
- [Bang for the Buck: Vector Search on Cloud CPUs — arXiv](https://arxiv.org/pdf/2505.07621)
