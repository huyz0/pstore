# Evaluation Methodology

**Answers:** Q30
**Status:** Complete (v1)

## The rule that governs all of it

> **Never report a latency number without stating cache state.** A "p50 of 10 ms" is
> meaningless unless we say whether it is warm, cold, or a realistic mix. Most vendor
> benchmarks in this category are warm-only and therefore useless for capacity planning.

Every result reports: **cold p50/p90/p99, warm p50/p90/p99, and the cold ratio of the mix.**

## Datasets

| Dataset | Size / dim | Tests |
|---|---|---|
| **SIFT1M / GIST1M** | 1M × 128 / 960 | Fast iteration, sanity, recall curves |
| **Deep1B** (Yandex) | 1B × 96 | Billion-scale; **low-dim, our SPANN weak spot** — include deliberately |
| **SIFT1B / BIGANN** | 1B × 128 | Classic billion-scale |
| **MS MARCO** | ~9M docs, 532k query-passage pairs | **BM25 and hybrid ranking quality** — the gold standard for IR evaluation |
| **YFCC-100M CLIP** | 98.7M × 1280 | **Filtered search** (the big-ann-benchmarks filtered track); also the standard filtered benchmark used by Pinecone |
| **LAION** subsets | up to 1B × 512/768 | Realistic modern embeddings |
| **Synthetic multi-tenant** | 1M indexes × 1k–1M docs | **Our unique regime — no public dataset exists. We must build it.** |
| **Synthetic correlated-filter** | — | GLS across [−1, 1]; correlated vs anti-correlated filters |

Use **`big-ann-benchmarks`** as the harness where possible — it has purpose-built tracks for
**filtered search, streaming search, and out-of-distribution search**, all three of which
match our differentiation claims.

## Metrics

### Retrieval quality
- `recall@k` (k = 1, 10, 100) against exact ground truth.
- `NDCG@10`, `MRR` for ranking (MS MARCO, and a labelled set of **50–200 queries with judged
  documents** per corpus type).
- **Filtered recall**: recall@k *among documents passing the filter*, swept across
  selectivity from 10⁻⁶ to 1, and across GLS correlation.

### Latency
- Cold / warm p50, p90, p99, p999, separately.
- **Sequential round-trip depth per query** — asserted ≤3 in tests (D-34).
- Time-to-first-result for streaming responses.

### Efficiency (the ones that differentiate us)
- **Blob requests per query** (W / Rseq / Rpar / List), per tenant.
- **Bytes fetched vs. bytes used** (speculative-fetch waste).
- **$ per 1M queries** and **$ per 1M writes**, computed from real request counters.
- Cache hit rate per class.
- CPU-seconds per query.

### Write path
- Ingest throughput per index and per cluster.
- Write latency p50/p99 in each durability mode.
- Time-to-searchable (write → visible in a strong-consistency query).
- **PUTs per million documents** — the number that decides whether the economics work.
- **PUTs per idle-ish index per month** — the tenancy floor. Measure with 100k+ trickle
  indexes; a single-index benchmark cannot see this and it is where the naive design fails.
- **Time-to-searchable** measured separately from write-ack latency, and **swept against the
  flush interval** — the two must be independent, or the freshness layer is not working.

### Scale and elasticity
- Cluster convergence time after adding/removing 1,000 nodes.
- **Cold-query ratio spike and decay after a 2× scale-out** — this is the honest cost of
  ownership-free elasticity and we should publish it.
- Behaviour at 1M+ indexes: open latency, catalog enumeration time, memory per idle index.

### Correctness under failure
- Deterministic simulation: N committers, injected pauses, partitions, 412/409/503 storms.
  **Assert Invariant I1** and linearizability of the epoch sequence.
- Jepsen-style external checking of the consistency claims in
  `03-metadata-consistency/consistency-model.md`.

## Baselines

| Baseline | Why |
|---|---|
| **turbopuffer** | The reference. Their published numbers (cold p50 874 ms / warm p50 14 ms @ 1M docs; write p50 165 ms @ 500 kB) are the bar. |
| **S3 Vectors** | The price floor and the ~100 ms warm ceiling. |
| **Qdrant / Weaviate** | Tier-1 quality and warm latency — we should be *close* warm and 20× cheaper. |
| **Pinecone Serverless** | The closest architecture. |
| **Exact brute force** | Ground truth for recall, and the honest baseline for small indexes. |

## Anti-patterns in benchmarking (things we must not do)

- Reporting warm-only latency. (Everyone does this. We won't.)
- Benchmarking a single index and calling it multi-tenant.
- Ignoring the write path.
- Recall measured on the same vectors used for training/clustering.
- Reporting QPS without stating recall — **QPS and recall are one number, never two**.
- Omitting blob-request cost, which is our actual product claim.
- Running on a cache pre-warmed by the ground-truth computation.

## Continuous evaluation

> **D-35.** Recall, ranking quality, round-trip depth, and blob-requests-per-query are **CI
> gates**, not periodic studies. A PR that silently drops recall@10 by 3% or adds a fourth
> round trip must fail the build.

Plus **production recall monitoring** (D-20): a sampled fraction of live queries also run
exactly, off the critical path, recording the recall delta per index. This is the only way to
detect distribution drift in a live multi-tenant system.

## Open questions raised

- OQ-72: Build the synthetic multi-tenant dataset generator — what distribution of index
  sizes and query rates is realistic? (Needs a power-law model; validate against any public
  data on SaaS tenant distributions.)
- OQ-73: How do we obtain ground truth for filtered recall at billion scale affordably?
- OQ-74: Can we publish a reproducible cost-per-query benchmark that competitors can run? It
  would be a strong marketing asset and a forcing function for honesty.

## Sources

- [big-ann-benchmarks — GitHub](https://github.com/harsha-simhadri/big-ann-benchmarks/blob/main/benchmark/datasets.py)
- [ANN-Benchmarks: A benchmarking tool for approximate nearest neighbor algorithms](https://www.researchgate.net/publication/331280060_ANN-Benchmarks_A_benchmarking_tool_for_approximate_nearest_neighbor_algorithms)
- [VIBE: Vector Index Benchmark for Embeddings — arXiv](https://arxiv.org/pdf/2505.17810)
- [MS MARCO: Benchmarking Ranking Models in the Large-Data Regime — arXiv](https://arxiv.org/pdf/2105.04021)
- [MS MARCO Web Search — arXiv](https://arxiv.org/pdf/2405.07526)
- [turbopuffer: fast search on object storage (published latencies)](https://turbopuffer.com/blog/turbopuffer)
